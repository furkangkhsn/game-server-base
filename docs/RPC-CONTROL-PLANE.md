# gsb: RPC Deseni ve Kontrol Düzlemi — Tasarım Notları

Bu doküman bu turun iki özelliğinin tasarım kararlarını yanıtlar:

- **A — Kontrol düzlemi:** idempotent runtime oda yaşam döngüsü
  (aç/kapa/durum), bilet-doğrulama kancası (asenkron I/O; base kancayı
  tanımlar, doğrulayıcı UYGULAMAZ), maç-sonucu çıkış dikişi (oda kapanınca
  rapor dışarı).
- **B — RPC deseni:** istemci isteği korelasyon numarası taşır; yanıt
  mevcut `private()` frame yoluyla; oda-local iş **aynı tick'te**,
  dış I/O'ya devredilen iş **sonraki tick'lerde** yanıtlanır; ikisi
  istemciye aynı şekli gösterir.

İki özellik de tek bir eksik mekanizma üzerine kuruludur: **tick dışı
asenkron tamamlama** ("işi devret, sonucu bağlantıya ilet"). Önce o
mekanizma, sonra her tasarım sorusunun cevabı.

Kod ve yorumlar İngilizce'dir; bu doküman Türkçe'dir (repo sözleşmesi).

---

## 1. Mekanizma: tick dışında asenkron iş, tick'te tek kavşağın sonucu

Oda actor'ü bir aktördür ve tek beklenecek kaynağı global tick
broadcast'idir (`tick_rx.recv()`); tick gövdesi **senkron** çalışır.
Bir istek yanıtı için I/O gerektiğinde (imza servisi, veritabanı,
HTTP — burada demo'da in-proc ekonomi servisi) tick gövdesi o I/O'yu
bekleyemez. Çözüm üç parça:

1. **İş (worker):** oda, `External` kararından gelen **sahiplenen**
   future'ı `tokio::spawn` ile ayrı bir görevde çalıştırır. Future'dan
   ayrıca bir `tokio::time::timeout(due - now, …)` **kaynak koruması**
   sarar (aynı süre sonu; §3). Worker görevi geleceği sonlandırırsa
   sonucu, sınırlı bir `completions` mailbox'ına bir mesaj olarak
   gönderir ve biter.
2. **Kavşak:** rapor mesajı tick'te **tek bir yerden** içeri girer:
   oda actor'ünün CONTROL fazı (0b) mailbox'ı **blok-etmeden** boşaltır
   (`try_recv` döngüsü) ve raporları `pending` tablosuyla **müzakere**
   eder (reconcile: bağlantı hâlâ odada mı? id hâlâ pending mi?).
3. **Yanıt:** müzakere edilen rapor, o tick'in `queued` haritasına
   düşer ve BROADCAST fazında normal `private()` frame'i olarak çıkar
   (`Private.responses`). Yanıtın şekli, aynı tick'te yanıtlanan
   oda-local yanıtla **birebir aynıdır** (korelasyon id'si, `ok`,
   iç op, `reason`, `payload`).

İki farklı "geç" kavramı karıştırılmamalı:

- **İstemciye görünür gecikmiş yanıt** (sürenin sonundan sonra
  istemciye ulaşan yanıt): imkânsız — koruma, sürenin sonunda
  future'ı **öldürür** (bkz. §3) ve süresi gelen id'yi süpürge
  yanıtlamıştır.
- **Geç gelen olay** (pending'den çıkmış bir id için worker raporu —
  ör. bağlantı istek uçuştaiken odadan ayrıldı): **olabilir**, normal
  bir durumdur ve müzakere onu **soğurur** — `requests_late` sayacı
  tam olarak bu soğurulan olayları sayar (bkz. §3/§4).

İstemciye görünür garanti: her id için **tam olarak bir** yanıt. Bu
garanti "geç olay olamaz"dan değil, **"geç olay ikinci yanıt üretemez"**
olguğundan gelir (koruma + süpürge süreyi kararlaştırır; müzakere
geci kalan her raporu atar). `requests_late` sayacının normal
işleyişte 0 **olmaması** beklenir (her uçuşta-istekli bağlantı
ayrılışı bir tane üretir); 0 okumak, müzakere yolunun çalışmadığı
anlamına gelir. Bu garantiye dayanıp müzakereyi "sadeleştirme"
yapmayın: soğurucu kaldırılırsa ilk ayrılan bağlantının uçuşta
istekleri istemciye ikinci yanıt olarak ulaşır.

"Tam olarak bir" yanıtın **teslimi** — yanıtı taşıyan batch'in
bağlantının dolu çıkış kanalında düşmesi — §3.1'dedir (F14).

## 2. Soru 1 (ana soru): Bekleyen (pending) durum NEREDEN?

**Karar: oda actor'ünün kendi yerelinde.**

```text
pending:        HashMap<ConnectionId, VecDeque<PendingRequest>>
pending_total:  u64              // oda genelinde açık istek sayısı
completions:    mpsc (cap = max_pending_requests.max(1))
queued:         HashMap<ConnectionId, Vec<RpcReply>>   // bu tick'te yayınlanacak
```

**Kapalı sınıf** (pending durumun tümlü parçaları — başka yerde başka
bir şey yoktur):

1. `PendingRequest { id, op, due }` — oda actor'ünün deque'ı:
   korelasyon + süre sonu (oda saatiyle).
2. **Çalışan future** — worker görevinin yerel durumu (bağlantı, id,
   future, koruma). Oda bunu **tutmaz**; spawn sonrası tek bağlantısı
   rapor mesajıdır.
3. **Yolda rapor** — `completions` kanalında (sınırlı; en kötü durum
   tam olarak o kadar açık istek).
4. **Yayını bekleyen yanıt** — `queued` haritası (BROADCAST'te
   tüketilir; tick sınırı kadar yaşar).

**Nerede değil (ve neden):**

- **Conn actor'ünde yok.** Conn actor'ü ince bir iletici: tek beklenecek
  kaynağı `inbox.recv()`; RPC_REQ frame'ini opak olarak odanın action
  mailbox'ına forward eder (sözlüğü çözmeden). Korelasyonu bilse,
  pending durumun **ikinci sahibi** olurdu ve ana sorunun cevabı ikiye
  bölünürdü.
- **Paylaşılan yapısında yok.** Kilit yok, atomik yok; durum tek
  görevde (oda actor'ü) yaşar — mimarinin temel kuralı.

**Neden oda (kavılabilen tek yer):** müzakere, üç şeyin **aynı senkron
tick fazında** birlikte görülmesini ister: (a) rapor mesajı,
(b) bağlantının hâlâ odada olması, (c) sürenin dolup dolmadığı. Bunu
gören tek aktör odadır. Ayrıca korelasyon bağlantı başına *ama karar
oda başınadır*: cap'ler (`max_pending_requests*`), `request_timeout`
ve handler (`handle_request`) hepsi `RoomConfig`/`RoomLogic`
alanındadır. Oda, görünüme açık zaman otoritesidir de (tick saati).

**Reddedilen alternatifler (en az iki):**

1. **Worker doğrudan conn'ın çıkış kanalına yazsın.**
   Reddedildi. (a) Aynı istemci akışına **iki görev** yazardı: odanın
   broadcast'i ve worker'ın yanıtı — frame sıralaması belirsiz hale
   gelirdi (oysa bugün tek yazıcı: oda). (b) Worker, bağlantının
   canlılığını, cap'leri ve dup'ı kendisi bilecek — oda otoritesini
   kaybederdi. (c) Süre sonu **iki otoriteye** bölünürdü (worker'ın
   koruması + birinin süpürmesi) ve "tam olarak bir yanıt" kanıtı
   yıkılırdı.
2. **Rapor conn actor'ünün inbox'ından dolaşsın** (worker → conn inbox →
   conn, odaya forward). Reddedildi. (a) Conn actor'ü, açık isteklerin
   tüm ömrü boyunca raporları tutar — pending durumun **ikinci yeri**
   olur (ana soruya iki cevap). (b) Bağlantı kapanınca eski raporları
   atmayı **conn actor'ü** bilecekti — ince ileticiye korelasyon
   bilgisi yüklenirdi. (c) Aynı en kötü durumu boyutlandırması gereken
   bir mailbox daha (hop + sınırlama maliyeti, otorite kazancı sıfır).
3. (Bonus) **Oda başına "completion dispatcher" actor'ü.** Reddedildi.
   Oda başına spawn/kill edilmesi gereken fazladan bir aktör;
   "registry oda bekleyemez" kuralı bir aktör daha fazla büyürdü; ve
   0b fazı zaten pending durumun sahibi olan görevde mailbox'ı
   boşaltıyor — dispatcher sadece hop ekler, otorite eklememiştir.

## 3. Soru 2: Süre sonu (timeout) KİME AİT? İstemci ne görür?

**Tek süre sonu, iki zamanlayıcı — otorite tek:**

- **Worker'ın koruması:** `tokio::time::timeout(due - now, future)`.
  Bir **kaynak korumasıdır, yanıt otoritesi değil**: sürenin sonunda
  future'ı öldürür (yavaş bir backend worker görevini sonsuza dek
  tutamaz — görev sızdırmaz) ve süresi dolarsa **hiçbir şey rapor
  etmez**. Rapor etseydi, süpürmeyle yarışır ve id başına iki yanıt
  çıkabilirdi.
- **Odanın süpürmesi (süpürge, 0b fazı):** her bağlantının deque'ında
  **baştan** bakılır (süre sonları FIFO'da artan — yeni kayıt daha
  ileri; bu yüzden baş kontrolü yeterlidir, iç tarama gerekmez).
  Süresi gelen kayıt atılır ve istemciye
  `ok=false, reason="request timed out"` yanıtı kuyruğa konur.

**Neden böyle bölündü:** worker istemciye yanıt veremez (§2 —
alternatif 1); süpürge de yavaş backend'in görevini iptal edemez
(koruma yoksa görev süresinin çok ötesinde yaşardı). Tek "istemciye
görünür" otorite süpürge olduğundan, istemcinin gördüğü sözleşme
kesindir:

- Yanıt **şekil olarak diğer her reddiyle aynıdır** (id, `ok=false`,
  iç op, `reason`, boş payload). İstemci için "timeout" ile "reddedildi"
  aynı sınıf; neden metninde ayrılır.
- Koruma **aynı süre sonunda** ateşlendiğinden, sürenin sonundan sonra
  bir çözüm istemciye **ikinci yanıt olarak** ulaşamaz: worker, süpürmenin
  yanıtladığı id ile ikinci bir **istemciye görünür yanıt** üretemez.
  Dikkat — doğru garanti bu cümlenin *yanıt* kelimesindedir, "geç olay
  olamaz" ifadesinde değil: worker **raporu** gecikmiş olarak
  üretebilir (bağlantı arada ayrıldıysa pending kaydı silinmiştir;
  worker hâlâ koşar, süresi içinde çözer ve rapor eder). Müzakere bu
  raporu atar ve `requests_late` olarak sayar (bkz. §4) — sayaç, bu
  gecikmiş olayların **beklenen** bir sonucu olduğunu kanıtlar;
  "gecikme imkânsız" diye okumayın, müzakereyi de o varsayım üzerine
  sadeleştirmeyin. "Tam olarak bir yanıt" garantisi, koruma + süpürge +
  müzakere üçlüsünden gelir.

Süre sonu saati: kayıt anındaki `Instant` + `RoomConfig.request_timeout`
(demo/varsayılan 5 sn). Oda saati = istemciye görünür otorite.

### 3.1 Teslim garantisi: düşen batch (F14)

**Yol.** Yanıtlar bağlantının `queued` girdisinde birikir; BROADCAST'in
bağlantı başına fan-out'u (oda faz 4d, shard faz 6d) girdiyi
`queued`'dan **çıkarır** (`replies_buf`), mantığın `private`'ına verir,
`private` onları bağlantının private karesine yazar, kare grup karesiyle
tek batch olur ve tek `try_send` ile bağlantının **sınırlı** çıkış
kanalına gider. Kanal doluysa (ya da kapalıysa) batch **bütünüyle**
atılır (`dropped_frames`). F14'ten önce o batch'in taşıdığı yanıtlar da
giderdi ve "istemci kabul edilmiş isteği asla beklemez" iddiası yalnız
istemcinin kendi zaman aşımına kalırdı.

**Düşmede çekirdeğin kaybettikleri** (incelendi): batch'te çekirdeğe
ait tek yük RPC yanıtlarıdır. Grup karesi paylaşımlı ve kendi kendine
yeter (bayatlığını keep-alive sınırlar), private karenin geri kalanı
(ack, tek seferlik full, oyunun oturum yükü) mantığındır ve F11'in
`on_batch_dropped`'ı ile yeniden kurulur. Hata/kontrol yanıtları
(auth, join, sürüm, ihlal kapanışı) fan-out'a binmez: conn actor'ünün
kendi `send().await`'i ile gider, düşmez.

**Kural (şimdi garanti edilen).** Düşen batch'in taşıdığı yanıtlar
bağlantının kuyruğunun **başına** geri konur ve sonraki tick'te
mantığın `private`'ına, sonraki yanıtlardan **önce**, yeniden verilir —
kanalın kabul ettiği ilk batch'e kadar. **Tam olarak bir kez, sırayla:**
yanıt kuyruktan yalnız alınırken çıkar, yalnız düşmede geri döner;
teslim edilen yanıt tekrar gönderilmez (sessiz bir tick'in düşmesi hiç
yanıt taşımaz — `replies_buf` son teslim edilenleri tutsa bile). Yeni
kanal, kilit, await yok; sessiz yol aynı (tek `is_empty` yoklaması,
düşme yokken ayırma yok). Hiçbir şey düşmediğinde istemci baytı birebir
aynıdır. Dolayısıyla: **kabul edilmiş bir isteğin yanıtı, bağlantı
boşaldığı anda o bağlantıya ulaşır.**

**Fırtına sınırı.** Bağlantı **tıkalıyken** (son batch'i düştü —
`RoomConn.dropping`), bir istek ancak bağlantının borcu — kuyruktaki
(taşınanlar dahil) yanıtlar + uçuştaki (pending) istekler —
`max_pending_requests_per_conn`'dan azsa kabul edilir. Sınırdaki istek
**reddedilir: işlenmez, yanıtlanmaz** (kendi sayacında sayılır,
`requests_refused_congested` — F15, aşağıda). Denetim, kabulün yanıt borcu
doğuracağı her yerdedir: 2c'de her istek (dup kontrolünden önce), 2a'da
bozuk zarf. Yanıtsız ret tek sınırlı seçenektir: her yanıt — ret dahil
— bir teslim edilmemiş yanıt daha olurdu. İstek uygulanmadığından
istemcinin kendi zaman aşımından sonraki tekrarı güvenlidir.

**Kesin sınır.** Tıkalıyken borç yalnız kabulle büyür ve kabul yalnız
borç < cap iken olur; tamamlanma/süpürme pending'i `queued`'a taşır
(toplam değişmez). Düşme dizisi başladığında borç ≤ o ana kadarki
pending (≤ cap) + o tick'in çekilen istekleri (≤ bağlantı başı çekim
bütçesi — istek de action'dır). Yani bağlantı başına:

> teslim edilmemiş yanıt + uçuştaki istek ≤ `max_pending_requests_per_conn`
> + `max_actions_per_conn_per_tick` (varsayılan 4 + 16 = **20**).

Oda genelinde bu, tıkalı bağlantı sayısıyla çarpılır (pending kısmı
ayrıca oda cap'iyle sınırlı). Tıkalı olmayan bağlantı hiç reddedilmez:
düşme yokken davranış aynıdır.

**Yanıtsız retlerin sayacı (BACKLOG F15).** F14'te bu retler
`requests_rejected_conn_cap` kovasına, istemcinin YANITINI gördüğü
sıradan cap retleriyle birlikte düşüyordu: operatör "bir bağlantı
kotasını dolduruyor" ile "bir bağlantı okumuyor, isteklerini yanıtsız
reddediyoruz"u ayıramıyordu — ikincisi yavaş okuyucu, birincisi istemci
davranışı sorusudur. Artık ayrı bir ÇEKİRDEK sayacıdır:
`RoomSample`/`RoomReport::requests_refused_congested` (oda ve shard
aktörü, 2a ve 2c yolları), `gsb-metric` satırında `req_refused=`
(`req_rej_room=`'dan sonra), Prometheus'ta
`gsb_room_requests_refused_congested_total`, loadgen metrik telinde
`GSMD` (GSMC + `requests_rejected_room_cap`'ten hemen sonra alan; SUM
ile katlanır), `RESULT`'ta `req_refused=`. `req_rej_conn` artık yalnız
yanıtlanan cap retleridir. Elenenler: (1) *Kovada bırakmak* — ayrım
yapılamıyordu (sorunun kendisi). (2) *`conn_cap` ailesine sebep etiketi*
(`{reason="congested"}`) — var olan bir ailenin şeklini değiştirir,
altı kovanın "kova başına aile" düzenini bozar ve yanıtlanmamış bir
isteği bir "ret" ailesinde tutar. (3) *F9 mantık sayacı* — karar
çekirdeğindir, mantığın değil; F9 sayaçları oyunun adlandırdıklarıdır.
(4) *Yedinci `requests_rejected_*` kovası adı* — istemci bir ret
görmedi; ad istemcinin gördüğünü söylemeli (`refused`: yanıt yok).
Testler: oda ve shard fırtına testleri (`fanout::replies::bound`,
`shard::tests::replies::bound`) yanıtsız retleri yeni sayaçta VE
örnekte sayar, `req_rej_conn`'u 0 tutar; `tests/rpc/buckets.rs`'in her
kova testi yanıtsız ret sayacının kıpırdamadığını da ister; metrik
altın metni (`metrics::tests::golden`) tam bu anahtarı ve aileyi
kazandı (değeri 3 olan bir odayla); loadgen `wire`/`rules` testleri tel
ve katlamayı sabitler. Mutasyonlar (dört artış noktasının her biri
eski kovaya, örnek/rapor/satır/Prometheus/codec/fold eşlemeleri) her
biri en az bir testi düşürür.

**Yük altında** (B23'ün loadgen RPC modu, §8.2): 500 istemci × 10
istek/sn, 3 sn'lik duraklamalarla 137 746 düşen batch boyunca ikinci ya
da eşleşmeyen yanıt 0; kabul edilmiş her yanıt en geç duraklama + ~2
tick'te geldi; fırtına sınırı 36 837 isteği yanıtsız reddetti ve
istemcinin yanıtsız/açık sayısı tam bu retler + ayrılışta uçuşta
kalanlar.

**Hâlâ istemcinin zaman aşımına kalanlar.**

- **Oturumu önce biten bağlantı:** ayrılış (leave), detach — hiç
  boşalmayan istemcinin yazma-tıkanması sınırıyla (write-stall)
  kapatılması dahil — ya da başka shard'a göç, teslim edilmemiş
  yanıtları uçuştaki istekleriyle birlikte **götürür** (oturum
  kapsamlı RPC durumu; resume eden oturum ölü oturumun yanıtlarını
  almaz). Sızıntı yok: `drop_conn_request_state` ve fan-out sonundaki
  süpürme. **Sayılıyor (B53):** o iki yer, attığı yanıtları
  `requests_undelivered` (`req_undelivered=`,
  `gsb_room_requests_undelivered_total`), attığı uçuştaki istekleri
  `requests_abandoned` (`req_abandoned=`,
  `gsb_room_requests_abandoned_total`) olarak sayar. İkisi de defterin
  terimi DEĞİL — istek zaten kendi kovasında bir kez sayılı; bunlar
  yanıtın akıbetidir. Çift sayım yok: düşen ya da kapalı kanala çarpan
  batch'ten geri konan yanıt (kural yukarıda) `queued`'a süpürmeden SONRA
  döner, yani o tick sayılmaz; oturum bitince `drop_conn_request_state`
  onu bir kez sayar (kilit: `room::tests::fanout::replies::undelivered`,
  `shard::tests::replies::undelivered` — kapalı kanala üç kez çarpıp geri
  konan tek yanıt ayrılışta 1). Uçuştaki isteğin worker'ı sonra rapor
  ederse o rapor ayrıca `req_late`'te sayılır (rapor sayacı). Yanıt
  defteri böylece kapanır: `req_local + Σ req_rej_* + (req_ext −
  req_abandoned − req_pending)` yanıt kuyruğa girdi; bunlardan
  `req_undelivered`'ı teslim edilmedi. Oda KAPANIRKEN kuyrukta/uçuşta
  kalanlar sayılamaz: oda dururken son örnek göndermez (§8.3, elenen 5).
- **Fırtına sınırında reddedilen istek:** hiç kabul edilmedi, yanıtı
  yok.
- **Yanıtları kodlamayan mantık:** `private` verilen `responses`'ı
  yazmazsa (batch boşsa) yanıt düşmeden önce kaybolur — mantığın
  sözleşmesi (`GameLogic::private`).

## 4. Soru 3: Korelasyon id uzayı, dup/stale, bütçe

- **Uzay:** `u64`, **bağlantı başına** (o odadaki üyelik süresince),
  **istemci tarafından atanır**. `id = 0` saklıdır → normal ret
  (malformed ile aynı yol; korelasyona uğrayamaz).
- **Görülen küme (seen-set) YOKTUR.** Çalışan küme, `pending` deque'ı
  **kendi başınadır**. Yanıtlanan id **anında yeniden kullanılabilir**
  (sınırsız geçmiş tutulmaz; bellek, cap'lerle sınırlıdır — §6).
- **Dup (çalışan id tekrarı):** aynı id hâlâ pending'ken ikinci bir
  istek — hangi karar türü olursa olsun (Reply, Reject, External) —
  **işlenmeden** reddedilir (aynı tick, normal ret, `requests_rejected_dup`).
  Karar türüne göre değil karardan ÖNCE kontrol edilir: bu turda yakalanan
  gerçek bir oda hatası tam olarak buydu (oda-local bir istek, çalışan
  bir id'yi yeniden kullanarak **ikinci** yanıt alıyordu; test
  `duplicate_inflight_id_rejected_then_reusable` bunu kilitler).
- **Stale:** (a) Bağlantı açıkken odadan ayrılırsa deque'u **bütün
  olarak** atılır (slotlar aynı anda boşalır); (b) bir rapor, id
  yanıtlanmış veya bağlantı gitmişken gelirse, 0b müzakeresi onu
  `requests_late` sayacıyla **atar**. İkisi de normal durumdur —
  istemciye "geç yanıt" asla gitmez.
- **Bütçe (en kötü durum):** bağlantı başına 4 açık istek
  (env + yanıt kuyruğu; çıkış kanalı tıkalı bağlantıda teslim edilmemiş
  yanıtlar dahil en çok 4 + 16 — §3.1), oda başına 2000 `PendingRequest` + 2000 worker
  görevi + 2000 slot'lu `completions` kanalı (varsayılanlar; §6'daki
  türetim). Cap aşımı = aynı tick'te normal ret (§6). Yük altında
  (§8.2): 500 × 10 istek/sn'de ~250 uçuşta — oda cap'i hiç bağlamadı;
  B=8'lik patlamada bağlantı cap'i her patlamanın tam yarısını reddetti,
  istemci ve oda aynı sayıyı gördü.
- **Ret sayaçları nedene göredir** (bu turda tek `requests_rejected`
  yerini altı kovaya bıraktı — `req_rej_malformed / _dup / _no_handler
  / _logic / _conn / _room`; tıkalı bağlantının yanıtsız retleri F15'ten
  beri ayrı: `req_refused`, §3.1): her kova farklı bir operasyonel soruya
  cevap verir (istemci protokol hatası mı, dup fırtınası mı, tek
  bağlantı mı, oda bütçesi mi, oyun mantığının normal iş ret'i mi).
  Oda cap'inin pratikte bağlayıp bağlamadığı — yani 2000'in doğru
  sayı olup olmadığı — yalnız bu kovalardan ölçülür.

## 5. Soru 4: Aynı tick'te action + request SIRASI

Tick faz sırası sabittir:

```text
READ (tüm action'lar, sınırlı çekim) → 2a AYRIŞTIR → 2b CONVERT (tüm
action'lar) → 2c (tüm istekler) → SYSTEMS → BROADCAST
```

- Bir istek, **bu tick'in action'ları uygulandıktan SONRA** dünyayı görür.
- Her sınıf içinde geliş sırası korunur (2a, in-order `retain_mut` ile
  ayrıştırır — action ve istek listeleri geliş sırasını saklar).
- **Gerekçe:** action "ateşle-unut" mutasyondur; istek, en güncel durumu
  gözlemlemesi gereken sorgu/etkidir. İstemcinin doğal sırası — aynı
  tick'te önce "hareket" sonra "yetenek" — yeteneğin hareketi görmesini
  ister. İstekler önce işlenseydi, action'dan SONRA gelen bir istek,
  action'dan ÖNCEKİ dünyayı görürdü — istemcinin gözleyebileceği bir
  yarış.
- Sessiz tick maliyeti: çekilen action başına tek bir `u16` karşılaştırma
  (2a'nın tek sabit maliyeti; 0b'de boş `try_recv`, `queued` için tek
  `is_empty` sorgusu). Ölçüm sonucu §8'de.

## 6. Soru 5: Bağlantı başına pending cap'i (ve neden oda bakar)

- `max_pending_requests_per_conn` (varsayılan **4**) ve
  `max_pending_requests` (varsayılan **2000**) `RoomConfig` alanlarıdır
  ve **2c'de, odada** denenir — çünkü pending durumun sahibi odadır
  (§2). Conn actor'ü pending'i hiç saymaz (bilmez bile).
- **Amaç:** tek bir istemcinin oda bütçesini doldurabilmesini
  sınırlamak. Cap ile en kötü durum bağlantı başına 4'tür.
- Cap aşımı, normal reddir (aynı tick; nedene göre sayaç — §4) —
  istemci "dolu" olduğunu öğrenir ve kendi zamanlayıcısıyla tekrar
  deneyebilir. **İstisna (F14):** çıkış kanalı tıkalı bağlantıda cap,
  teslim edilmemiş yanıtları da sayar ve sınırdaki istek yanıtsız
  reddedilir (§3.1 "Fırtına sınırı").
- Ticket doğrulaması için eşdeğer sınırlama **yapısal**dır: bağlantı
  başı en fazla **bir** doğrulama çalışır (actor, oneshot'ta park
  ederken ikinci AUTH_REQ inbox'ta bekler — sayaç gerekmez; bkz. §7).

### 6.1 Boyutlandırma kuralı (bu turda düzeltildi)

**Cap = beklenen istek hızı × backend gecikmesi + pay. Nüfusa göre
değil.** Uçuştaki (in-flight) istek sayısı Little yasasıyla belirlenir:
`L = λ × T`. Örnek türetim: 10 000 oyuncu × oyuncu başına dakikada 1
istek (λ ≈ 167/s) × 1 sn backend gecikmesi → **~167 uçuşta**; ~12×
pay → **2000** (varsayılan). Popülasyonla ölçeklemeyin: "10 000
oyuncu var → 40 000 cap" yanlıştır — 10 000 üyeli ama sakin bir oda
küçük cap ister, 100 üyeli ama yavaş backend'e bağlı bir oda büyük cap
ister; nüfus doğrudan girdi değildir.

- **Bağlantı başına 4'ün türetimi:** meşru bir istemcinin aynı anda
  1–2 isteği uçuştadır (mantıksal aksiyon başına bir; aynı karede
  iki-aksiyon patlaması gerçekçi üst sınırdır); + 1–2 **retry payı**
  (yanıt gelmeyen istek için istemci yeni id ile tekrar sorar; eski
  slot cevabını alana dek doludur). 4 = 2 + 2. 2, patlama + retry'ı
  reddeder; daha büyük değer tek sorunlu bağlantının oda bütçesinden
  alabileceğini büyütür — adalet düğmesi zaten alttaki eşiğindedir.
- **Tükenme eşiği (türetilmiş sayı):** oda cap / bağlantı başına cap =
  2000 / 4 = **500 bağlantı**. Oda cap'i ancak 500+ bağlantı aynı anda
  tam kota doluyken bağlar — 10 000 üyeli odada nüfusun **%5'i**.
  Adalet özelliği bu sayıdadır: eşiğin altında hiçbir bağlantı alt
  kümesi odanın pending bütçesini tek başına tüketemez. (Eski değerler
  256/16 = **16 bağlantı** = 10 000 üyeli odanın %0.16'sı, kalan 9 984
  kişiyi aç bırakabilirdi.)
- **Sınır (bu turda dokümana girdi):** bu cap'ler istek **sayısını**
  sınırlar (odanın kendi durum bütçesi), **backend eşzamanlılığını
  değil**. 2000 uçuşta istek, backend'e en fazla 2000 **eşzamanlı**
  çağrı demektir. Kapasitesi sınırlı bir servise gidiyorsanız
  (örn. 32 eşzamanlılık) çağıran tarafında kendiniz throttle edin
  (kendi kuyruk/havuzunuz): base, servisinizin kapasitesini ne biler ne
  de bilmelidir.
- **Doluluk-türetilmiş cap:** bilinçli olarak **yapılmadı** (statik
  varsayılan + config korundu): 500'lük eşik, 10 000 üyeli odada bile
  nüfusun %5'i altında kaldığı sürece adaleti sağlıyor; türetme, B'deki
  ret kovalarının verisi (oda cap'i gerçekten bağlıyor mu) gelene kadar
  kör bir değişiklik olurdu.

## 7. Bilet doğrulaması: bağlantı durumu ve bütçe etkileşimi

**Kanca:** `TicketValidator = Arc<dyn Fn(Bytes) ->
Pin<Box<dyn Future<Output = Result<ValidatedTicket, TicketError>> + Send>>
+ Send + Sync>`; `TicketAuth { validator, timeout }`; composition root'a
`ServerHooks.ticket: Option<TicketAuth>` olarak verilir. `None`
(default) = eski local-auth yolu, bayt-bayt değişmez. **Base hiçbir
doğrulayıcı uygulamaz** (imza servisi çağrısı, önbellek … platforma
özeldir — taşımadaki adapter bölünmesinin aynısı).

**Doğrulama sırasında bağlantı durumu:** AUTH_REQ handler'ı, local-auth
yolundaki SpawnPlayer idyomunun aynısıyla çalışır: worker görevi
(spawn) + **tek oneshot** bekleme. Park ederken actor başka frame
işlemez (tek-bekleme disiplini): heartbeat/join kuyruğa (sınırlı inbox)
yığılır ve auth çözüldükten sonra işlenir. Pencere **yapısal olarak
sınırlıdır**: koruma, kancanın `timeout`'unda ateşlenir → oneshot `Err`
çözer → actor devam eder. Taşıma katmanındaki idle-sonu saatinden
bağımsızdır (ayrı katman, `ServerClosed` mesajı).

**Doğrulama başarısızlığı: ihlal DEĞİL, normal reddir.**
Hatalı/boş/bekleyen bilet → `ERROR` kod **10**, **ağırlık 0**, ihlal
bütçesine **hiç girmez**, bağlantı **hayatta kalır**. Gerekçeler:

- Bilet **istemci verisidir** (oyun-seviye girdi), protokol ihlali
  değil. İhlal bütçesi, protokol bütünlüğü içindir (bilinmeyen op,
  boyut aşımı); bilet hatası meşru olabilir (süresi dolmuş, iptal
  edilmiş, login sunucusu hatası).
- Bağlantıyı kapatmak, bir **veri** sorununda yeniden bağlantı
  zorlar; bütçelemek, istemcinin kendi bütçesini çöp biletlerle
  tüketmesine (kendi kendine DoS) izin verir. İkisinden de kaçınmak:
  normal ret, bağlantı canlı.
- Oda sabitlemesi (ticket bir oda taşır) farklı odadaki JOIN →
  `ERROR` kod **11**, yine normal ret — ve **local**dir (conn actor'ü,
  registry turundan ÖNCE yakalar; gereksiz round-trip yok).

**Gerçek doğrulayıcı ve sayım (B21, docs/TICKETS.md).** Kanca değişmedi;
hazır bir doğrulayıcı opt-in `gsb-ticket` crate'iyle gelir (PASETO
v4.public, oyunun taleplerine göre jenerik, `Validator::into_auth` tek
satır) ve sunucu `[ticket]` tablosundan da kurar (`ticket` feature).
Base yine biçim bilmez: oyun kendi doğrulayıcısını takabilir. Eklenen iki
şey ek ve varsayılanlı: (1) her AUTH kararı tek sebeple sayılır —
doğrulayıcı sebebi `TicketError::Refused(TicketReason)` ile, oyunun
kontrolü `TicketError::Game(GameReason)` ile adlandırır; serbest metinli
`Rejected` `other`, zaman aşımı `timed_out`, boş bilet `missing`, ölen
worker `validator_lost` sayılır; (2) `ValidatedTicket::extra` (oyunun
doğrulanmış talepleri) tanımlı katılımla oda/shard katılım kancalarına
ve ev yönlendiricisine taşınır. Ret yolu aynen normal rettir (kod 10,
ağırlık 0, bağlantı canlı).

**Tick senkronluğu:** doğrulama **conn actor'ünde** yaşanır, odada
değil — oda tick gövdesi bir doğrulamayı **hiçbir zaman** beklemez.
E2E testi bunu kilitler: 250 ms'lik bir doğrulama uçuşundayken, odada
olan oyuncunun snapshot akışı tick hızında devam eder.

## 8. Ölçüm: RPC/ticket yolları tick süresine dokunmuyor

N=500, `step_p50_fine_us`, release, `taskset -c 8-15` (8–15
çekirdek pin), 30 sn = 900 adım, makine yükü (loadavg) her çalıştırmada
kayıt; kirli koşu atılır. Komut birebir aynı:
`gsb-loadgen 500 --duration 30 --still-frac {0.0,0.9}` (in-proc,
visibility=all, profile=ring, tcp).

Karşılaştırma **A/B** yapıldı: bazel binary (a11c14f, ayrı worktree'de
derlendi) ve yeni binary, **aynı makine durumunda dönüşümlü**
koşuldu (her koşudan önce 1-dk loadavg kaydedildi); makine yükü
koşu anında 3.5–7 aralığındaydı ve koşular arası değiştiği için
mutlak sayılar değil, aynı koşullardaki ikili fark okunur.

**still_frac=0.0** (4 koşu):

| binary | koşu | loadavg (başlangıç) | p50 fine | p90 fine | max | over-budget |
|---|---|---|---|---|---|---|
| baz (a11c14f) | 1 | 5.56 | 296 µs | 432 µs | 749 µs | %0.0 |
| baz (a11c14f) | 2 | 5.83 | 320 µs | 424 µs | 733 µs | %0.0 |
| yeni | 1 | 6.89 | 312 µs | 432 µs | 730 µs | %0.0 |
| yeni | 2 | 5.03 | 312 µs | 440 µs | 1014 µs | %0.0 |

**still_frac=0.9** (4 koşu):

| binary | koşu | loadavg (başlangıç) | p50 fine | p90 fine | max | over-budget |
|---|---|---|---|---|---|---|
| baz (a11c14f) | 1 | 4.05 | 256 µs | 352 µs | 742 µs | %0.0 |
| yeni | 1 | 3.99 | 240 µs | 408 µs | 678 µs | %0.0 |
| baz (a11c14f) | 2 | 3.59 | 272 µs | 376 µs | 701 µs | %0.0 |
| yeni | 2 | 2.82 | 272 µs | 400 µs | 824 µs | %0.0 |

**Yorum:** `step_p50_fine_us` (spec metriği) bazel ile aynı
aralıkta: bazın kendi koşular-arası değişimi (296↔320, 256↔272 —
24 µs) kadar. still=0.9'da yeni binary bazla eşit veya daha düşük
(240/272 vs 256/272); still=0.0'da fark ±16 µs ve yükle
korelasyonlu (yüksek loadavg → yüksek sayı, her iki binaryde).
p90/max kuyruk yüzdelikleri bu ortamda zamanlayıcı gürültüsü
egemen (aynı binarynin koşuları max'ta ±150 µs geziniyor) — yapısal
bir fark göstermiyor; `step_over_budget_pct` **tüm koşularda %0.0**,
`actions_dropped` tüm koşularda 0.

**Önemli düzeltme (ölçüm sırasında bulundu):** ilk implementasyon,
broadcast'te bağlantı başına `queued.remove(conn)` yapıyordu (sessiz
oda için 500 probe/tick) — aynı makine durumunda 360–416 µs
ölçmüştü; `is_empty` korumasıyla (sessiz yol O(1)) 296–312 µs'e
döndü. Yani asıl "yeni maliyet" o probe'lardı ve giderildi; kalan
sabit maliyet (action başına tek `u16` karşılaştırma, boş
`completions`'a tek `try_recv`, tek `is_empty` probe) ölçüm
gürültüsünün altında kaldı.

Kayıtlar: `.measure/ab_*.log` (tam RESULT satırları + loadavg).

### 8.1 Cap turu: değer değişikliği sessiz yola dokunmuyor (bu tur)

Cap'ler 16/256 → 4/2000 yapıldı (gerekçe §6.1). Cap değerleri yalnız
istek yolunda (2c) okunduğundan, RPC trafiği **olmayan** 500-istemci
koşusunda sessiz tick yolu değişmemeli. Doğrulama: aynı komut
(`gsb-loadgen 500 --duration 30 --still-frac 0.9`, release,
`taskset -c 8-15`), eski ve yeni binary, dönüşümlü, koşu başına loadavg
kayıtlı; kontamine koşu atılmadı (loadavg 4.9–6.4 aralığında, masaüstü
taban yüküyle uyumlu — argos + tarayıcı, user testi görünmedi).

| binary | koşu | loadavg (başlangıç) | p50 fine | p90 fine | max | over-budget |
|---|---|---|---|---|---|---|
| eski (16/256) | 1 | 5.86 | 336 µs | 424 µs | 1541 µs | %0.0 |
| eski (16/256) | 2 | 4.91 | 352 µs | 504 µs | 815 µs | %0.0 |
| yeni (4/2000) | 1 | 6.36 | 368 µs | 456 µs | 1223 µs | %0.0 |
| yeni (4/2000) | 2 | 6.10 | 328 µs | 464 µs | 1060 µs | %0.0 |

p50 ortalaması 344 → 348 µs (+4 µs, %1.2 — koşular-arası gürültü;
eşleştirme farkları +32/−24, yük ile korelasyonlu); over-budget tüm
koşularda %0.0, `metrics_dropped=0`, `steps=900` (30.00 Hz). Sessiz
yol değişmedi — beklendiği gibi. Kayıtlar: `.measure/cap_before{1,2}.log`,
`.measure/cap_after{1,2}.log` (tam RESULT satırları; yeni satır
`req_*` kuyruğunu da içeriyor — bu turun D maddesinin smoke
assert'leri o alanlara dokunur).

### 8.2 Yük altında RPC yolu: loadgen RPC modu (B23)

Bu turdan önce loadgen hiç istek göndermiyordu: odanın istek alımı,
bağlantı başına pending cap'i, ret kovaları, worker tamamlanmaları ve
zaman aşımları, F14'ün düşen batch'ler üzerinden teslimi ve fırtına
sınırı yük altında hiç koşmamıştı.

**Mod.** `gsb-loadgen N --rpc-rate R [--rpc-burst B]`: her istemci
girdilerinin yanında demo'nun `ECONOMY` isteğini (`BuyItem { kind:
"potion" }` — desenin dış-I/O yarısı: pending slot, worker görevi,
ekonomi servisi, sonraki tick'te completion) B'lik patlamalarla, her
B/R saniyede bir gönderir (ortalama R istek/sn; B varsayılan 1).
Takvim join'de başlar (join'den önceki istek oda action'ı değildir), id'ye
göre faz kaydırılır (istemciler aynı anda patlamaz), kaçan dilim telafi
edilmez (hız bir tavandır). Korelasyon id'leri oturum başına `1, 2, 3, …`
(yeniden kullanılmaz — her yanıt tam bir isteği adlandırır ya da hiçbirini).

- **Yalnız demo:** barındırılan oyunlardan istek işleyicisi olan tek
  oyun; diğerleri her isteğe "no handler" der — `--rpc-rate` /
  `--rpc-burst` başka oyun için demo bayrakları gibi reddedilir.
- **Yalnız düz istemci koşusu** (süreç içi ya da `--addr`):
  `--orchestrate`, `--serve`, `--churn-secs` ile kullanım hatası
  (orkestratörün `CLIENT` satırları defteri taşımıyor; `--serve`'in
  istemcisi yok; churn istemcisinin oturumları tek defter değil).
  `--rpc-burst` `--rpc-rate`'siz reddedilir.

**Defter** (`client/rpc/ledger.rs`, istemci başına). Yanıtlar her private
karenin 3 numaralı alanından okunur (`Private.responses` — kit'in ve her
oyunun private mesajında aynı numara):

- bekleyen isteğe gelen **ilk** yanıt onu kapatır; türüne göre bir kez
  sayılır: `ok`, ya da nedeninin adlandırdığı çekirdek reddi (sunucunun
  zaman aşımı, bağlantı cap'i, oda cap'i, dup, handler yok, malformed),
  ya da oyunun kendi reddi (başka her neden); `ok` ise gecikmesi
  (gönderim → varış) kaydedilir;
- kapanmış isteğe ikinci yanıt **dup** (`rpc_dup_answers`), gönderilmemiş
  id'ye (0 — malformed yanıtı — ya da son id'nin ötesi) yanıt
  **eşleşmeyen** (`rpc_unmatched`); ikisi başka yerde sayılmaz ve ikisi de
  0 olmalı (§1'in "tam olarak bir yanıt"ı, §3.1'in tam-bir-kez teslimi);
- istemcinin kendi zaman aşımı = sunucunun istek zaman aşımı
  (`RoomConfig::request_timeout`, 5 sn; sunulan sunucu değiştirmiyor) +
  1 sn pay. Sınırdan sonra gelen yanıt isteğini yine kapatır ama **geç**tir
  (`rpc_late`); koşu sonunda hâlâ bekleyen istek sınırdan yaşlıysa
  **yanıtsız**, gençse **açık** (`rpc_open` — bitişte uçuşta,
  yargılanmaz). İstemci tarafı zaman aşımı = yanıtsız + geç
  (`rpc_client_to`).

Ret nedenlerinin metinleri artık `gsb_core::rpc` sabitleridir
(`TIMEOUT_REASON`'ın yanında `CONN_CAP_REASON`, `ROOM_CAP_REASON`,
`DUPLICATE_REASON`, `MALFORMED_REASON`, `NO_HANDLER_PREFIX` +
`no_handler_reason(op)`): oda ve shard bunlarla yanıtlar, loadgen bunlarla
sınıflar — üçüncü bir el kopyası yok. Metinler bayt bayt aynı (birim testi
`rpc::tests::the_rejection_reasons_are_pinned` sabitler); istemci baytı
değişmedi.

Bu modda yavaş okuyucu (`--stall-ms`) okumazken de gönderir (bir sonraki
patlama uykusunu bitirir): F14'ün fırtına sınırı ancak tıkalı bağlantı
istek göndermeye devam ederken sınanır. Yalnız yanıt taşıyan private kare
(`PrivateEvent::Empty` + yanıt) artık hata sayılmaz.

**RESULT.** Anahtarlar yalnız modda ve `game=`'den hemen önce (diğer
isteğe bağlı segmentler gibi; modsuz satır birebir aynı, mevcut her
anahtar yerinde): `rpc_rate rpc_burst rpc_sent rpc_ok rpc_to rpc_rej_conn
rpc_rej_room rpc_rej_dup rpc_rej_no_handler rpc_rej_malformed
rpc_rej_logic rpc_client_to rpc_late rpc_open rpc_dup_answers
rpc_unmatched rpc_ok_p50_ms rpc_ok_p99_ms rpc_ok_max_ms` (gecikmeler
yalnız `ok` yanıtların, ham değerler üzerinden, ms). Karşılarında odanın
kendi sayaçları her satırda zaten var (`req_ext`, `req_rej_*`,
`req_refused`, `req_unread` — B36, §8.3 —, `req_to`, `req_late`): aynı
yolun iki ucu yan yana.
İnsan-okunur rapora bir `rpc (clients): …` satırı; dup/eşleşmeyen > 0 ise
`WARNING`.

**Ölçüm** (release, süreç içi, TCP, 32 çekirdek; makine kardeş çalışma
ağacının derlemeleriyle paylaşımlı — parantezde koşudan hemen önceki 1 dk
yük ortalaması). Komut: `gsb-loadgen N --duration 30 --write-stall-secs 0
--rpc-rate R [--rpc-burst B] [--stall-ms P --stall-every-ms E --conn-out 4]`.
Her koşuda `joined = left = N`, `errors = server_closes = 0`,
`server_hz = 30,00`, `req_to = req_rej_room = 0`,
**`rpc_dup_answers = rpc_unmatched = 0`**.

| Koşu (yük) | sent | ok | rej_conn istemci / oda | refused (oda) | client_to | open | ok p50 / p99 / max ms | step p50 / p90 µs | dropped |
|---|---|---|---|---|---|---|---|---|---|
| 200, R=1 (17,1) | 5 968 | 5 958 | 0 / 0 | 0 | 0 | 10 | 50,9 / 68,2 / 73,0 | 232 / 352 | 0 |
| 200, R=10 (19,1) | 59 302 | 59 264 | 0 / 0 | 0 | 0 | 38 | 52,8 / 101,5 / 155,8 | 640 / 1368 | 0 |
| 500, R=1 (32,9) | 14 631 | 14 594 | 0 / 0 | 0 | 0 | 37 | 50,8 / 68,2 / 85,9 | 800 / 1392 | 0 |
| 500, R=10 (22,8) | 145 890 | 145 693 | 0 / 0 | 0 | 0 | 197 | 49,4 / 67,9 / 85,8 | 1408 / 1904 | 0 |
| 500, R=10, B=8 (26,3) | 146 584 | 73 248 | **73 292 / 73 292** | 0 | 0 | 44 | 53,7 / 71,0 / 76,5 | 752 / 1224 | 0 |
| 500, RPC'siz (18,6) | — | — | — | — | — | — | — | 512 / 776 | 0 |
| 200, R=10, duraklama 1000/5000 (30,6) | 59 378 | 59 330 | 0 / 0 | 0 | 0 | 48 | 54,7 / 1004,6 / 1067,5 | 376 / 504 | 0 |
| 500, R=10, duraklama 1000/5000 (30,3) | 146 250 | 145 970 | 0 / 0 | 0 | 0 | 280 | 53,9 / 1003,3 / 1068,1 | 1112 / 1768 | 10 913 |
| 200, R=10, duraklama 3000/6000 (15,2) | 57 624 | 53 511 | 0 / 0 | **3 926** | 3 576 | 537 | 65,5 / 2999,9 / 3086,5 | 448 / 744 | 19 081 |
| 500, R=10, duraklama 3000/6000 (24,3) | 142 250 | 104 714 | 0 / 0 | **36 837** | 29 504 | 8 032 | 57,1 / 3008,4 / 3068,3 | 1200 / 1456 | 137 746 |

Tick maliyeti için aynı makine durumunda dönüşümlü A/B (500 istemci,
`--duration 20`, sıra RPC'siz / R=10 / R=3, iki tur): step p50 RPC'siz
728 · 736 µs (yük 15,3 · 10,2), R=3 992 · 1016 µs (11,2 · 7,9), R=10
1472 · 1480 µs (14,3 · 9,6); `dup_answers` 0.

**Okuma.**

1. **Tam bir kez yük altında tutuyor.** 137 746 düşen batch'li koşu
   dahil hiçbir koşuda ikinci ya da eşleşmeyen yanıt yok. İki uç her
   kovada aynı sayıyı görüyor: patlama koşusunda istemcinin cap retleri
   odanınkiyle birebir (73 292), ve hesap kapanıyor: `sent = req_ext +
   req_refused` (500'lük uzun duraklama: 105 413 + 36 837 = 142 250) ve
   istemcinin yanıtsız + açık'ı = odanın yanıtsız retleri + ayrılışta
   uçuşta kalan kabul edilmişler (`req_ext − ok`): 29 504 + 8 032 =
   36 837 + 699.
   200'lük uzun duraklamada 62 istek (`sent − req_ext − req_refused`)
   hiçbir oda sayacına düşmedi (500'lük koşuda 0). B36'da iki nedeni
   doğrulandı ve kapandı — oda ayrılıştan hemen önce gelen istekleri
   okumadan atıyordu (artık `req_unread`), loadgen odanın sayaçlarını
   son periyodik örnekten okuyordu (artık bir periyot bekliyor): §8.3.
2. **Bağlantı cap'i tasarlandığı gibi bağlıyor, oda cap'i bağlamıyor.**
   B=8'lik patlama tek tick'te çekiliyor (bağlantı başı çekim bütçesi 16):
   her patlamanın tam 4'ü kabul, 4'ü cap retti (146 584'ün yarısı).
   Oda cap'i hiçbir koşuda bağlamadı (`req_rej_room = 0`): 500 × 10/sn ×
   ~50 ms ≈ 250 uçuşta ≪ 2000 — §6.1'in Little yasası kuralı ölçüldü.
3. **Gecikme ~1,5 tick.** `ok` p50 ≈ 50 ms, p99 ≈ 68 ms: istek bir
   sonraki tick'in READ'ini bekler (ortalama yarım tick), ekonomi 5 ms,
   completion bir sonraki tick'in 0b'sinde, yanıt o tick'in
   BROADCAST'inde. Sunucu zaman aşımı hiç yok (`req_to = 0`).
4. **İsteğin kendi tick maliyeti ilk kez ölçüldü: dış istek başına
   ~4,5–5,5 µs** (A/B: 1500 istek/sn'de +~270 µs, 5000 istek/sn'de
   +~745 µs; 500'lük odada tick başına 50 / 167 istek). Zarf çözme,
   handler (`BuyItem` çözme, servis tutamacının klonu, future'ın kutusu),
   pending kaydı, worker `tokio::spawn`'ı, completion uzlaşması ve yanıtın
   private kareye kodlanması. 33 ms'lik bütçenin çok altında
   (`over_budget` %0, 30 Hz). §8'in "sessiz yol dokunulmadı" sonucu
   değişmedi — bu, trafiğin kendisinin maliyeti. §11'deki worker havuzu
   spawn kısmını sınırlardı.
5. **F14 teslimi ölçüldü.** Kısa duraklamada (1 sn / 5 sn) 500'de batch'ler
   düşüyor (10 913) ama bağlantı duraklama başına yalnız ~4–6 tick tıkalı
   kalıyor (çekirdek tamponları geri kalanını emiyor): borç hiç 4'e
   varmıyor, ret 0. Düşen batch'lerin taşıdığı yanıtlar tam bir kez geliyor,
   en geç duraklama + ~70 ms'de. Uzun duraklamada (3 sn / 6 sn) fırtına
   sınırı çalışıyor: 3 926 / 36 837 yanıtsız ret, istemcide yanıtsız ya da
   açık olarak görünüyor — başka hiçbir kovada değil. Kabul edilmiş
   isteğin yanıtı en geç duraklama + ~2 tick'te (max 3 086 ms): "bağlantı
   boşaldığı anda ulaşır" (§3.1) ölçüldü. Reddedilen isteği istemci
   yalnız kendi zaman aşımıyla görür (§3.1'in tasarımı): istemcinin
   zaman aşımı en uzun duraklamasının ötesinde olmalı.

**Aynı turda B32'nin yeniden ölçümü** (`gsb-loadgen --orchestrate 500
--procs 2 --write-stall-secs 0 --duration 20`, üç koşu, yük 16,8 / 13,4 /
9,5): `dropped` 116 / 116 / 116 — tekrarlanıyor (önceki kayıtlar 116,
116, 123, 116). Ama **katılma fırtınasında değil**: orkestratörün aldığı
raporların zaman çizgisi (geçici tanı, commit'lenmedi) katılmada ve
kararlı pencerede 0, hepsi **ayrılış** penceresinde (adım 600 → 630,
`members` 500 → 0). Geçici bir çekirdek tanısı 116'nın hepsinin
`try_send` → **Closed** olduğunu, bağlantı başına tam bir kez, gösterdi:
istemci LEAVE sonucunu alır almaz soketini kapatır, yazıcı biter ve
bağlantının çıkış kanalı kapanır; oda ayrılışı registry üzerinden bir
sonraki tick'inde öğrenir, arada fan-out o kapalı kanala bir batch daha
dener ve `dropped` sayar — istemcinin istediği bir kare kaybolmuyor.
Sayı zamanlamaya bağlı: aynı topoloji elle (`--serve` + 2 × 250 `--addr`)
varsayılan worker'larla 0, sunucu `--workers 1` → 116, istemciler
`--workers 1` → 80, ikisi 1 → 130, sunucu 2 + istemciler 1 → 1.
Tekrarlanabilirliğin kaynağı: pinsiz orkestratör çocuklara
`--workers 1` veriyordu (`args.workers.max(1)`; yorum "runtime default"
diyordu) — B37'de düzeltildi, aşağıda (B50, 2026-09-28: varsayılan
worker'larla üç koşuda `dropped` 0, `sends_closed` 0; bugünkü kod
`--workers 1` ile yine tam 116, artık `sends_closed`'ta). Kapalı kanalı `dropped`'tan
ayırmak ayrı bir karardı (metrik anlamı değişikliği, B32) — **sonradan
yapıldı** (bakımcı kararı 2026-09-27, "her şeyi saymalıyız"):

**B32: kapalı kanal `dropped` değil.** Fan-out'un bağlantı başı
`try_send`'i iki türlü düşer ve artık ikisi ayrı sayılır
(`room::SendFailures` — oda ve shard aktörünün BROADCAST fazı aynı
yardımcıyı kullanır, iki yol ayrışamaz): **Full** — çıkış kanalı dolu,
yavaş istemci, canlı bağlantıya giden batch kaybı — `dropped`
(`gsb_room_dropped_total`, HELP'inin dediği gibi); **Closed** — bağlantı
zaten gitmiş, oda sonunu henüz işlememiş — yeni `sends_closed`
(`RoomSample`/`RoomReport::sends_closed`, `gsb-metric` satırında
`sends_closed=` `dropped_s=`'den sonra, Prometheus'ta
`gsb_room_sends_closed_total`, OTLP'de `gsb_room_sends_closed`, loadgen
metrik telinde `GSMI` (GSMH + `dropped_s`'ten hemen sonra alan; SUM ile
katlanır), `RESULT`'ta `sends_closed=` `dropped=`'den sonra, her
satırda). `dropped`'ın `_s` oran göstergesi var, `sends_closed`'ın yok:
bağlantı sonlarıyla orantılı olduğundan oranı ayrılış oranının bir
katıdır, yeni bilgi taşımaz. Bağlantı sonu başına tek bir değil (F59
düzeltmesi; ilk yazım "en çok ~1" diyordu): (o bağlantıya tick başına
denenen batch) × (soketin kapanışıyla ayrılışın/kopuşun işlenmesi
arasındaki tick) — ayrılış registry → oda (shard'lı odada registry →
shard) yolunu dolaşırken her tick'in fan-out'u kapalı kanala bir batch
daha dener. Ölçülen: spatial demo 1000 `--workers 1` ile 1,6–1,8,
shard'lı MMO/savaş varsayılan worker'larla 0,9–1,9 (aşağıda "B50").
Batch'in işlenişi değişmedi (tampon geri alınır, `on_batch_dropped`
çağrılır, taşıdığı RPC yanıtları kuyruğa geri konur — bağlantı
gitmişse bir sonraki tick satır silinince düşer).
Kilit: `room::tests::fanout::dropped::closed` ve
`shard::tests::dropped::closed` (önce kırmızı: kapalı kanallı bir üye +
dolu kanallı bir üye, iki tick → `dropped` 3, beklenen 1; `sends_closed`
2). İstemci teli değişmedi. Yukarıdaki orkestre 500'ün 116'sı ve
aşağıdaki tabloların `dropped` sütunu B32'den önce alındı: 116'nın
hepsi bugün `sends_closed`'a düşerdi; duraklama koşularının binlerce
düşüşü dolu kanaldır (kapalı kanal bağlantı sonu başına birkaç ekler — F59).

**Pinsiz orkestratörün worker sayısı (B37, düzeltildi).** Orkestratör
(`--orchestrate`, `--pin` olmadan) sunucu ve istemci çocuklarına
`--workers args.workers.max(1)` veriyordu: operatör `--workers`
yazmadıysa (`0` = "sorulmadı") her çocuk **tek worker'lı** bir tokio
çalışma zamanında koştu. Yorum "runtime default" diyordu; orkestratör
ilk yazıldığında (`559c745`) bu doğruydu — o gün `0` her modda tek
worker demekti — ama regresyon ölçüm turu (`227cc04`) düz ve `--serve`
modunun varsayılanını `available_parallelism`'e çekince orkestratör
geride kaldı. Şimdi (`child_args.rs::child_workers`): `--pin` altında
çocuk kendi çekirdek kümesinin boyunu alır (değişmedi, `--workers`'ı
ezer); pinsiz ve operatör sayı verdiyse `--workers N` iki çocuğa da
iletilir; pinsiz ve sayı yoksa `--workers` **hiç iletilmez** — çocuk
kendi varsayılanında (`available_parallelism`, `main.rs`) koşar, yani
tek bir varsayılan var, orkestratör onu kopyalamıyor. Kilit:
`child_args::tests::{unpinned_children_keep_their_runtime_default,
an_explicit_worker_count_reaches_both_children,
a_pinned_child_gets_its_core_count}` (önce kırmızı: `Some("1")`).

*Etkilenen tabanlar — bu turda yeniden ölçülmedi* (B50'de ölçüldü,
aşağıda "B50: pinsiz orkestre tabanları varsayılan worker'larla").
`--pin`'siz ve
`--workers`'sız her orkestre koşusu tek worker'lı süreçlerde alındı;
sayıları olduğu gibi duruyor, "o koşul altında" okunmalı, yeniden
ölçüm B37'nin tetikleyicisiyle (orkestre ölçümü yeniden alınırken)
gelir. Belgelerdeki yerleri:

- bu bölüm: "Aynı turda B32'nin yeniden ölçümü" (orkestre 500,
  `dropped` 116 — tek worker'ın ürünü; elle varsayılan worker'larla 0);
- DESIGN §5.7 (B29 WS ölçümü, "Orkestre demo 500") ve §6 "El sıkışan
  kapılar" (B31 ölçüm tablosunun "orkestre 500" satırları);
- GAME-MODULE "G3 sonucu": demo A/B tablosunun `--orchestrate 1000
  --procs 2` satırı ve "İlk ölçüm tabanları"nın 1000'lik (`sep`)
  satırları; "W2 sonucu" → "Tabanlar"ın 1000'lik satırı;
- CROSS-SHARD §8b.8 (W2 rölesi, 1000 `sep`);
- KIT-ARCHITECTURE "T sonucu" (arena 1000 orkestre A/B satırı ve
  `clients_cpu_s` +%15 notu) ve "W2 sonucu" ("Yük altında", 1000);
- CHANGELOG'un G3, T ve W2 turlarının 1000'lik kayıtları, ROADMAP'in
  "1000 istemci orkestre" tabanı ve BACKLOG A24 (aynı sayılar).

Etkilenmeyenler: `--pin`'li orkestre koşuları (CROSS-SHARD "Ölçümler
(release, orchestrator --procs 4 --pin …)", CHANGELOG'un C1 aynası ve
10 000'lik `--pin` koşuları — çekirdek kümesinin boyu iletiliyordu) ve
tek süreçli (süreç içi / `--serve`) koşular.

**B50: pinsiz orkestre tabanları varsayılan worker'larla (2026-09-28,
`73da266`).** Yukarıdaki listenin her orkestre koşusu kayıttaki komut
satırıyla yeniden alındı, bu kez çocuklar kendi varsayılanında
(`available_parallelism` = 32). Makine: AMD Ryzen 9 7950X 16C/32T,
`nproc` 32, release; bu turda makinede başka ölçüm/derleme işi yoktu
(masaüstü taban yükü ~3; yükün 1 dk ortalaması < 5'e inene dek
beklendi, kabul edilen koşularda 2,3–4,95; 17,1'de koşan bir demo 1000
koşusu atıldı ve yeniden alındı). Yapılandırma başına üç koşu (medyan,
parantezde en küçük–en büyük). Ayrıca aynı ağaçta `--workers 1` ile
(orkestratör açık sayıyı iki çocuğa da iletir — B37 öncesi koşulun
bugünkü koddaki karşılığı) ikişer koşu: eski sayılarla fark "worker mı,
kod mu" sorusunu ayırır. Tabloda "tek worker'lı çocuklar (B37 öncesi)"
sütunu kayıttaki sayılardır.

| Taban (komut) | Metrik | tek worker'lı çocuklar (B37 öncesi, kayıt) | bugün `--workers 1` | **bugün varsayılan worker'lar** |
|---|---|---|---|---|
| demo 500 TCP (`--orchestrate 500 --procs 2 --write-stall-secs 0 --duration 20`) | step p50/p90 fine µs | 248/432 · 240/448 | 352/632 · 296/528 | **808/1216** (712–832 / 1200–1224) |
| | `server_cpu_s` / `clients_cpu_s` | 1,7 / — | 2,3 · 1,9 / 6,5 · 5,4 | **4,6** (4,4–4,7) / **11,9** (11,4–12,1) |
| | `dropped` / `sends_closed` | 116 (×7, B32 öncesi; hepsi kapalı kanal) | 0 / **116 · 116** | **0 / 0** (×3) |
| | connect p50/p99 ms | 17/1024 · 20/1009 · 18/1054 · 15/1053 | 21/1056 · 19/1035 | **31/1066** (28–33 / 1042–1072) |
| | `out_bps_per_conn` | — | 122 211 · 121 719 | **121 000** (120 741–121 212) |
| demo 500 WS (aynı, `--transport ws`) | step p50/p90 fine µs | 256/416 · 272/416 | 320/528 · 320/512 | **816/1272** (768–896 / 1208–1336) |
| | `server_cpu_s` / `clients_cpu_s` | 1,9 · 1,8 / — | 2,2 · 2,1 / 5,8 · 5,5 | **4,9** (4,8–5,2) / **11,7** (11,2–12,3) |
| | `dropped` / `sends_closed` | 14 · 17 (B29), 10 · 29 → 0 · 0 (B31) | 0 / 0 | **0 / 0** |
| | connect p50/p99 ms | 34/1075 · 36/1075 (B31 sonrası) | 33/1065 · 47/1017 | **32/1064** (29–34 / 1057–1075) |
| demo 1000 spatial (`GSB_LOADGEN_CLIENT_LINES=1 … --orchestrate 1000 --procs 2 --visibility spatial --duration 8`) | `joined` | 1000 | 1000 · 1000 | **988 · 997 · 869** (B84) |
| | `snap_total` | 223 958–226 282 | 225 163 · 224 260 | 215 545 (188 351–215 869) |
| | `client_in_bps` / `out_bps_per_conn` | 63,3–64,7 M / 64 090–65 444 | 64,1 M · 63,9 M / 64 864 · 64 743 | 59,8 M (45,7–59,8 M) / 59 563 (45 532–59 633) |
| | `acks` / `gap_drops` | 44 263–44 815 / 856–877 | 44 864 · 44 756 / 861 · 868 | 43 015 (37 556–43 223) / 663 (631–765) |
| | step p50/p90 fine µs | 992–1288 (p50) | 1032/1552 · 712/1208 | **2048/2944** (1776–2088 / 2424–3208) |
| | `server_cpu_s` / `clients_cpu_s` | — / 4,8–6,6 | 1,8 · 1,5 / 4,9 · 4,5 | 3,3 (3,0–3,5) / 8,1 (6,2–8,5) |
| arena 1000 (`--orchestrate 1000 --procs 2 --game arena --duration 10 --write-stall-secs 0`) | step p50/p90 fine µs | G3: 1632/2024; T HEAD: 2304/2960 … 3512/4096 | 1784/2168 · 1672/2072 | **1984/2928** (1936–2184 / 2816–3784) |
| | `out_bps_per_conn` | G3: 234 120; T HEAD: 207 246–213 521 | 115 152 · 113 657 | **110 800** (109 872–111 471) |
| | `dropped` / `sends_closed` | G3: 1761; T HEAD: 1035–1332 (B32 öncesi) | 0 / 1639 · 1800 | **0 / 0** |
| | `server_cpu_s` / `clients_cpu_s` | G3: 2,8 / 8,1; T HEAD: 3,8–4,9 / 14,6–16,2 | 2,6 · 2,4 / 8,3 · 7,8 | **4,0** (3,9–4,2) / **13,9** (13,5–14,5) |
| mmo 1000 (aynı, `--game mmo`) | step p50/p90 fine µs (max) | 288/400 (1540) | 280/392 · 296/408 | **440/648** (432–472 / 632–704; max 1780–4772) |
| | `out_bps_per_conn` / `snap_total` | 102 190 / 280 645 | 97 150 · 96 721 / 282 548 · 281 470 | **95 175** (94 386–95 662) / 280 071 (278 956–281 489) |
| | `dropped` / `sends_closed` | 1836 (B32 öncesi) | 0 / 1507 · 1957 | **0 / 1301** (915–1621) |
| | `server_cpu_s` / `clients_cpu_s` | 2,3 / 7,2 | 2,1 · 2,2 / 5,2 · 5,2 | **4,5** (4,5–4,6) / **10,3** (9,8–10,4) |
| war 1000 (aynı, `--game war`) | step p50/p90 fine µs | 1448/2416 · 1736/3072 | 688/896 · 696/872 | **1104/2152** (1080–1120 / 2104–2216) |
| | `out_bps_per_conn` | 369 098 · 363 167 | 163 762 · 160 449 | **164 228** (163 149–164 559) |
| | `moves` / `ack_lag_max_ms` | 40 717 · 38 823 / 469 · 884 | 56 144 · 55 772 / 134 · 136 | **55 540** (55 129–55 607) / **104** (102–104) |
| | `dropped` / `sends_closed` | 1661 · 1908 (B32 öncesi) | 0 / 1189 · 1911 | **0 / 1687** (1656–1768) |
| | `server_cpu_s` / `clients_cpu_s` | 6,0 · 6,1 / 17,8 · 16,1 | 3,0 · 2,9 / 17,4 · 17,2 | **6,1** (6,0–6,2) / **30,8** (30,5–31,6) |

Her koşuda `errors = server_closes = 0`, `left = joined`, `server_hz`
29,98–30,00, `step_over_budget_pct` 0,0; war'ın röle sayıları CROSS-SHARD
§8b.8'de. Bayt sütunları eski kayıtla doğrudan kıyaslanmaz: G3/T/W2'den
sonra B37'den önce üç kodlama turu wire'ı küçülttü — A30 (kompakt wire
id: demo/MMO −%7–15), A31 (savaş paketli koşu: −%54), A10 (arena
`Ticks2`: −%43–45). `--workers 1` sütunu baytın worker sayısından
bağımsız olduğunu gösteriyor (farklar ≤ %3; demo 1000'deki düşüş eksik
katılımdan).

*Okuma.*

1. **B32'nin 116'sı tek worker'ın ürünüydü — doğrulandı.** Bugünkü kod
   `--workers 1` ile tam 116 `sends_closed` (iki koşuda da; `dropped`
   0) üretiyor, varsayılan worker'larla 0. Tek odalı oyunlarda
   (demo, arena) varsayılan worker'larla kapalı kanala gönderim hiç yok:
   ayrılış bir sonraki fan-out'tan önce odaya varıyor. Shard'lı
   oyunlarda (MMO, savaş) varsayılan worker'larla da 915–1768 — ayrılış
   registry → shard yolunu dolaşıyor. Hepsi kapalı kanal; `dropped`
   (dolu kanal) bütün orkestre koşularında 0. Not: `sends_closed`
   bağlantı sonu başına "en çok ~1" değil, bağlantı sonu başına
   (tick başına kare) × (soket kapanışıyla ayrılışın işlenmesi
   arasındaki tick) — spatial demo 1000'de `--workers 1` ile 1,6–1,8,
   savaşta 1,2–1,9 (F59).
2. **Varsayılan worker'lar adımı ~2–2,5×, sunucu CPU'sunu ~2,2× büyütüyor
   (demo 500); `server_hz` 30'da kalıyor.** Tek worker'lı çalışma
   zamanında fan-out'un uyandırdığı yazıcı görevleri aynı iş parçacığında
   sırayla koşar; 32 worker'la uyandırmalar çekirdekler arası (iş çalma,
   önbellek) ve oda görevi onlarla yarışıyor. Aynı yön süreç içi
   koşularda zaten vardı (DESIGN §5.7: süreç içi demo 500 800/1536
   µs ↔ orkestre tek worker 248/432). Savaş istisna görünüyor
   (1448/2416 → 1104/2152), ama o fark A31'in yarıya inen baytı:
   `--workers 1` bugün 688/896.
3. **İstemci süreçleri de ~1,7–2× CPU harcıyor** (`clients_cpu_s`,
   `--workers 1` ↔ varsayılan: savaş 1000 17,3 → 30,8 sn / 10 sn koşu,
   arena 8 → 13,9, MMO 5,2 → 10,3). Tek worker'lı istemci süreci tek
   çekirdekte doymaya yakındı; savaşın W2'deki doygunluğu (`moves`
   ~40 k, `ack_lag_max_ms` 469–884) ise asıl A31'in baytıyla gitti —
   bugün `--workers 1` ile de `moves` ~56 k, `ack_lag_max_ms` 134–136
   (varsayılanda 104). KIT-ARCHITECTURE "T sonucu"nun `clients_cpu_s`
   +%15'i (A24) tek worker'lı koşullarda ölçülmüştü.
4. **Katılma fırtınası dinleme kuyruğunu taşırıyor (B84).** Varsayılan
   worker'lı istemci süreçleri 1000 bağlantıyı neredeyse aynı anda
   açıyor; çekirdeğin `TcpExtListenOverflows` sayacı koşu başına 1000'lik
   koşularda 1082–1579 artıyor (`--workers 1`'de 372–844; 500'lük
   koşularda varsayılanla 257–411, `--workers 1` ile 121–132), connect p50 bazı koşularda ~1 sn'ye çıkıyor (SYN yeniden
   gönderimi). 8 sn'lik demo 1000 koşusunda bazı istemcilerin bağlantısı
   pencere boyunca sunucuya hiç kabul edilmiyor: istemci `connected =
   1000`, sunucunun registry'si `opens = joined + 1` (869–997); 10 sn'lik
   arena/MMO/savaş koşularında hepsi katılıyor. tokio'nun
   `TcpListener::bind` dinleme kuyruğu **128** (`somaxconn` 4096;
   *düzeltme, B84 turu:* burada 1024 yazıyordu — o mio 1.1 öncesinin
   değeriydi; mio ≥ 1.1 std'nin 128'ini veriyor, `ss -ltn`'in Send-Q'su
   da 128 gösteriyor. 1000 istemcinin 1024'lük kuyruğu taşırması zaten
   açıklanamazdı). Sunucu
   bir şey kaybetmiyor (hiç kabul etmediği bağlantı), ama kısa koşunun
   `joined`'ı ve `snap_total`'ı düşüyor — demo 1000 satırının `joined <
   N` koşuları bu yüzden.

**Regresyon denetimi (sayım turlarından sonra).** B32'den bu yana ~30
tur sıcak yollara sayaç ekledi. Üç ayrı kanıt:

- *Aynı makine hâlinde A/B* — sayım turlarından hemen önceki ağaç
  (`a73bc54`, B37 sonrası) `git archive` ile ayrı derlendi, `73da266`
  ile ABBA sırasında üçer koşu (yük 2,3–4,9):

  | Komut | `a73bc54` step p50/p90 (mean) · `server_cpu_s` | `73da266` step p50/p90 (mean) · `server_cpu_s` |
  |---|---|---|
  | orkestre demo 500 `--workers 1` | 336/560 (356) · 2,1 | 320/512 (310) · 1,9 |
  | orkestre demo 500 varsayılan | 704/1152 (677) · 4,2 | 688/1112 (678) · 4,3 |
  | süreç içi MMO 200 | 120/168 (127) | 120/168 (134) |
  | süreç içi demo 500 | 712/1224 (746) | 704/1312 (807; 635–844) |

  Bayt sayıları %1 içinde aynı; `a73bc54`'ün `dropped` 116'sı bugün
  `sends_closed` 116. Fark her satırda koşudan koşuya yayılımın içinde.
- *Süreç içi kayıtlar* (DESIGN §5.7 B29 tablosu, sayım turlarından
  önce; `gsb-loadgen N [--game G] --transport tcp --duration 20
  --write-stall-secs 0`, üçer koşu): demo 200 step 184/272 (kayıt
  200/320 · 200/304), `out_bps_per_conn` 46 556 (46 690 / 46 408);
  demo 500 712/1256 (800/1536 · 696/1312), 119 053 (119 393 / 119 405);
  arena 200 408/560 (384/488 · 400/520), 23 491 (23 588 / 23 445); MMO
  200 128/176 (112/160 · 104/176), 20 303 (20 254 / 20 264). §8.2'nin
  500 R=10'u (`--duration 30`, iki koşu): step 1576/2120 · 1416/1984
  (kayıt 1408/1904), `ok` p50/p99 49,3/68,0 · 49,4/68,0 (49,4/67,9),
  `dup_answers = unmatched = 0`; RPC'siz 808/1528 · 672/1224.
  Tek > %10 sapma MMO 200'ün p50'si (+16–24 µs, ince histogramın bir
  kovası) — yukarıdaki A/B'de iki ağaç aynı (120/168).
- *Pinli tabanlar* (CHANGELOG'un C1 aynası): `--orchestrate 5000
  --procs 4 --pin --visibility spatial --cell-size 5 --profile ring
  --stagger-ms 0.5 --duration 30`, üç koşu: step p50 (kaba) 12,5 · 12,5
  · 25 ms (kayıt 12,5 ms), mean 13,8 · 13,8 · 16,9 ms, max 44,6–54,7 ms
  (C1: 42,9), bütçe aşımı %0,5 · %0,6 · %2,9 (%1,0), `server_hz`
  29,97–29,99, `server_cpu_s` 87,7–90,3 (89,7), `dropped` 0 (9 086),
  `server_closes` 0. Aynı komut 10 000 ile iki koşu: p50 25 ms (kayıt 25
  ms), `server_hz` 29,96–29,98 (29,37), aşım %6,3–8,0, `late_max` 88–120
  ms (2,14 sn), `dropped` 0 (24 646), `server_cpu_s` 146,3–146,6. Tek
  oda `all` 10 000 (`--orchestrate 10000 --procs 8 --pin --duration 30
  --write-stall-secs 0`, iki koşu) karşılaştırılamaz — istemciler doyuyor
  (joined 9273 · 7144, kayıt 5934–7032; `outbound_dead` 129 · 199, kayıt
  67–172): p50 12,5 · 6,25 ms (6,25), aşım %5,3 · %1,5 (%2,6).

**Hüküm: sayım turlarından regresyon yok.** Kayıtla > %10 farkların
hepsinin nedeni kod değil: worker sayısı (B37), sonradan gelen kodlama
turları (A10/A30/A31) ya da makine hâli (aynı günün A/B'sinde eski
ağaç aynı sayıları veriyor). Ham çıktılar commit'lenmedi.

**B84: dinleme kuyruğu (2026-09-28).** Okuma 4'ün katılma fırtınası
motorun `listen_backlog` anahtarıyla (DESIGN §6, OPS §2) ve loadgen'in
`--listen-backlog N`'iyle (in-process ve `--serve` sunucusu;
orkestratör sunucu çocuğuna iletir, istemcilere değil) yeniden ölçüldü.
Komut B50'ninki: `GSB_LOADGEN_CLIENT_LINES=1 gsb-loadgen 1000
--orchestrate --procs 2 --visibility spatial --duration 8`, release,
çocuklar varsayılan worker'larla; her koşudan önce yükün 1 dk
ortalaması < 5 beklendi (kabul edilen koşularda 2,5–4,9; makinede başka
bir projenin testleri koşuyordu, yük inene dek beklendi). Beş
yapılandırma sırayla, üç tur (her turda sırayla bir koşu). Taşma sayısı
`/proc/net/netstat`'ın `TcpExtListenOverflows`'unun koşu boyunca
farkı (sistem geneli); etkin kuyruk koşu sırasında `ss -ltnp`'nin oyun
kapısı için Send-Q'su.

| Yapılandırma | etkin kuyruk | `ListenOverflows` | `joined` | connect p50 / p99 ms | `snap_total` |
|---|---|---|---|---|---|
| `main` @ `21460c5` (değişiklikten önce) | 128 | 1228 · 1080 · 1235 | 990 · 1000 · 938 | 49/2078 · 1014/2084 · 49/2059 | 216 640 · 215 045 · 206 953 |
| bu tur, varsayılan | 128 | 1611 · 997 · 1268 | 852 · 1000 · 989 | 1028/1068 · 41/1077 · 1039/2083 | 186 460 · 220 150 · 209 702 |
| bu tur, `--listen-backlog 1024` | 1024 | **0 · 0 · 0** | **1000 ×3** | 11/47 · 14/42 · 15/45 | 231 194 · 231 864 · 231 624 |
| bu tur, `--listen-backlog 4096` | 4096 | **0 · 0 · 0** | **1000 ×3** | 20/49 · 16/47 · 10/29 | 231 394 · 231 608 · 231 801 |
| bu tur, `--stagger-ms 1` (1 sn yayılma) | 128 | **0 · 0 · 0** | **1000 ×3** | 0/0 · 0/5 · 0/0 | 225 414 · 224 945 · 225 409 |

Her koşuda `connected = 1000`, `errors = server_closes = dropped = 0`,
`left = joined`. Kuyruk 128'deyken SYN kuyruğu da doluyor:
`TCPReqQFullDoCookies` koşu başına 90–356 (SYN cookie'leri devrede —
Linux'ta yarım açık istek kuyruğu da aynı değerle sınırlı); 1024/4096
ve stagger'da 0.

*Okuma.* (1) **Taşmanın nedeni 128'lik kuyruk** — 1000 istemcilik
fırtına onu koşu başına ~1000–1600 kez taşırıyor; kuyruk istemci
sayısını aşınca taşma 0, bütün istemciler katılıyor, connect p99 ~2 sn
→ < 50 ms. `snap_total` +%7–12 (eksik katılım ve geç katılanların
kaybettiği saniyeler bitti), `out_bps_per_conn` 44 774–62 298 → 69 019–
69 888; adım p50/p90 fine µs stagger'sız dört satırda aynı bantta
(1624–1880 / 2616–3608).
(2) **Varsayılan davranış değişmedi:** bu turun varsayılanı `main` ile
aynı bantta (etkin kuyruk ikisinde de 128; taşma 997–1611 ↔ 1080–1235,
`joined` 852–1000 ↔ 938–1000 — B50'nin 869–997'si). (3) **Stagger ayrı
bir soru:** 1 ms'lik yayılma fırtınayı kaldırıyor (connect ~0 ms), ama
patlamanın kendisini de — kabul yolunun en kötü hâli artık ölçülmüyor
ve yayılma pencereden yiyor (`snap_total` 225 k ↔ 231 k). Ölçüm
düzeneği için karar: fırtına ölçülmek isteniyorsa `--listen-backlog`
≥ istemci sayısı (çekirdek `somaxconn`'da keser); katılmaların zamana
yayıldığı bir oyun modellenmek isteniyorsa `--stagger-ms`. Loadgen'in
varsayılanı değişmedi (sunucu config'inin 128'i) — kayıttaki bütün
tabanlar aynı koşulda kalsın diye; kuyruğu büyütülmüş koşular bundan
sonra komut satırında `--listen-backlog` taşır.

**Sunucu çocuğunun portları (F31, düzeltildi).** Orkestratör sunucu
çocuğunun oyun ve metrik portlarını kendisi seçiyordu (`alloc_port`:
`127.0.0.1:0`'a bağla, numarayı oku, kapat, çocuğa `--bind`/
`--metrics-listen` ile ver). Kapatma ile çocuğun bağlanması arasında
portu başkası alırsa çocuk "Address already in use" ile ölüyordu (128
`yes` altında görüldü; asılma F25'te düzeltilmişti, yarış değil). Artık
süreç düzeni:

```text
orkestratör
  ├─ sunucu çocuğu  gsb-loadgen --serve --bind 127.0.0.1:0
  │                 --metrics-listen 127.0.0.1:0
  │     iki kapıyı bağlar, stdout'a tek satır basar:
  │     SERVING addr=127.0.0.1:P metrics=127.0.0.1:M
  ├─ istemci çocuğu 0  gsb-loadgen N0 --addr 127.0.0.1:P …
  └─ …
```

Orkestratör çocuğun stdout'unu okur (önceki satırlar — çocuğun
`tracing` çıktısı — kendi stdout'una aynen geçer, satırdan sonrası da),
satırı en çok 30 sn bekler (`SERVER_REPORT_BOUND`) ve istemci
çocuklarını satırın adresine yollar; metrik akışına bir kez bağlanır
(dinleyici satırdan önce bağlandı, yeniden deneme döngüsü kalktı).
Çocuk satırdan önce ölürse ya da sınırda susarsa (öldürülüp biçilir)
koşu orada biter: çıkış durumu 1, hata çocuğun çıkışını adlandırır
(`the server child exited before reporting its addresses (exit status:
1); no run`), hiçbir istemci çocuğu başlatılmaz. `--serve` elle
koşulduğunda da aynı satırı basar (`--bind 127.0.0.1:0` artık
kullanışlı); açık port veren komut satırları aynen çalışır. RESULT
satırı değişmedi. Kilit: `child_args::tests::no_child_is_told_a_port_picked_in_advance`,
`server_child::tests` (sahte çocuk `/bin/sh`: bildirdiği portlar
kullanılır; ölen/susan çocuk açık hata), `tests/loadgen_serve.rs`
(gerçek `--serve` `:0` ile satırı basar, iki kapı o portlarda açık) ve
`loadgen_smoke::an_orchestrated_run_whose_server_child_dies_still_ends`
(artık başarısız çıkış + istemci çocuğu yok).

**Kimsenin okumadığı sunucu çocuğu (F40, düzeltildi).** `--serve
--metrics-listen` ile başlayan çocuğa metrik akışı için kimse
bağlanmazsa (elle koşulan `--serve`, ölen bir orkestratör) çocuk
`--duration`'dan sonra çıkmıyordu: dışa aktarım görevi (`metrics_export`)
`accept`'te sonsuza dek bekliyordu (F31'den önce de böyleydi; orkestratör
hep bağlandığı için yalnız elle koşuda ve düşen testte görünüyordu).
Artık ana görev `stop()`'tan sonra dışa aktarımı tek bir süre sınırı
altında bekler (`serve.rs::EXPORT_STOP_GRACE` = 2 sn, pump deyimi) ve
aşarsa keser: `serve: the metric stream was not taken (or not read)
within 2s of the stop; its reports are dropped`, çıkış 0. Akışı almış bir
okuyucu son raporu milisaniyeler içinde alır (rapor `stop()` döndüğünde
kanalda; birkaç KiB'lık yerel yazma); sınır yalnız kimsenin almadığı ya
da okuyucusu okumayı bırakmış bir akışı bitirir. Elenen: dışa aktarımın
kendi `accept`'ine `--duration`'dan türeyen bir son tarih (duruşu
bilmez; geç bağlanan orkestratör raporların hepsini yine alabilir, çünkü
raporlar kanalda birikir — ana görevin sınırı duruştan itibaren sayar).
Kilit: `tests/loadgen_serve.rs::a_served_server_nobody_reads_still_exits`
(`--duration 1`, kimse bağlanmaz; önce kırmızı: 21 sn'lik asılma
korumasına takıldı; sınırı 3600 sn yapan mutasyon da).

**Testlerde idle penceresi kapalı (F52).** `loadgen_rpc.rs`'nin
koşuları `--idle-timeout-secs 0` ile: her oturumu istemcisi bitirir
(defterin bağlantı terimleri 0 iddia edilir), pencere yalnız yanlış
ateşleyebilir. Süreç içi sunucu ve istemcileri aynı süreçtir; pencereden
(30 sn) uzun süren bir süreç takılması (swap, iki tam kapı aynı anda)
her pump'ı idle son tarihi dolmuş uyandırır — dört oturum idle diye
kapanır, her ayrılış kaybolur: A7'nin son koşusundaki `left=0
server_close_idle_timeout=4`. Loadgen'i 2 sn'lik pencereye karşı 3 sn
dondurmak bunu her seferinde üretir (3/3); pencere kapalıyken 3/3
`left=4`. Pencere iddia olduğu yerde (e2e, pump testleri) sınanır.
Aynı turda iki gecikme iddiası da yük altında düşüyordu (aç sunucu
12 Hz'de cevabı 800 ms'ye dek geciktirdi; tam ikilinin beş koşusunun
üçü): `the_rooms_ledger_covers_the_end_of_the_run` oda defterini TAM
okur (`req_ext + req_rej_conn + req_unread = rpc_sent`; 10/s'de 400 ms'yi
aşan cevap dört bekleyen sınırına çarpar, ret iki tarafta aynı sayılır);
`every_request_is_answered_once_at_a_sane_rate`'in "makul hız"
iddiaları (istemci başına en çok bir uçuşta, ret yok, istek sayısı)
koşunun kendi kanıtından okunur — en yavaş cevap istek aralığının
(200 ms) içindeyse; değilse koşu yinelenir, üç kanıtsız koşu gecikmeyi
söyleyerek düşer. İki defter her koşuda kesin eşitlikle sınanır.

### 8.3 Ayrılışta okunmamış istekler ve oda defterinin kapanışı (B36)

§8.2'nin açık kalan boşluğu: 200'lük uzun duraklama koşusunda 62 istek
hiçbir oda sayacına düşmüyordu (`sent − req_ext − req_refused`). İki
ayrı neden çıktı; ikisi de kapandı.

**Neden 1 — oda, ayrılıştan hemen önce gelen istekleri okumadan
atıyordu (motor).** Odanın tick'i CONTROL'ü (faz 0) READ'den (faz 1)
önce koşar. İstemcinin `LEAVE_ROOM_REQ`'i bağlantı actor'ünde action
kanalını bırakır ve registry → bağlantı başı dispatcher üzerinden odaya
`RoomControl::Leave` olarak gider. Son READ'den sonra gönderilmiş
istekler hâlâ o kanaldayken ayrılış odaya varırsa faz 0 satırı siler
(`despawn_conn`) ve kanal içindekilerle birlikte düşer: istek ne işlenir,
ne yanıtlanır, ne de herhangi bir kovada sayılırdı. İstemci her isteği
gerçekten göndermiş (`errors = 0`, `left = N`); istemci tarafında bunlar
`rpc_open`'a düşer.

- *Kanıt* (`gsb-core/tests/rpc/unread.rs`): bağlantı 1 yanıtlanmış bir
  istekten sonra aynı tick penceresinde üç istek + bir düz action
  gönderir ve ayrılır; bağlantı 2'nin bariyer isteği örneği alır.
  Düzeltmeden önce test kırmızıydı: kovaların toplamı 2 (yanıtlanan +
  bariyer, ikisi de `req_local`), gönderilen 5 — üç istek hiçbir yerde.
- *Boyut.* Bitiş (`deadline`) tüm istemciler için aynı an; kaybolanlar
  son patlaması "son READ ile ayrılışın odaya varışı" arasındaki
  pencereye düşen istemcilerin istekleri — beklenen ≈ N × pencere /
  patlama aralığı (200 × ~31 ms / 100 ms ≈ 62). Pencere koşudan koşuya
  0 ile ~1 tick arasında: düzeltmeden sonra 200 istemci, R=10 koşularında
  `req_unread` 0, 0, 10, 2, 87 (aşağıdaki tablo) ve uzun duraklama
  koşusunun bir tekrarında 32.

**Düzeltme 1 (motor katmanı).** Yeni çekirdek sayaç
`requests_dropped_unread`: oturumun action kanalını götüren her yerde
kanal önce kapatılır (`close()` — geç gönderim olmaz, boşaltma kanal
kapasitesiyle sınırlı), içindeki RPC istekleri sayılır ve kanal düşer
(`room::drop_unread`; B36'da `drop_unread_requests`). Düz action'lar
B36'da sayılmıyordu; sayım turu 2'den (B54) beri yanındaki ayrı sayaçta
sayılıyor (`actions_dropped_unread`, aşağıda).
Yerler, oda ve shard aktöründe: `despawn_conn` (ayrılış, despawn eden
kopuş, despawn'la biten hold), aynı bağlantının yeniden katılımının
süpürdüğü bayat satır, resume (park edilmiş satırın ölü kanalı yenisiyle
değişir — park edilmiş satır READ'de atlandığından istekleri resume'da
ya da despawn'da sayılır; AI devrindeki `bot_fed` satır da satır bitince),
shard'da ayrılıştan sonra gelip epoch kapısında düşen göçün taşıdığı
kanal ve — sayım turunda eklendi — girdi-boşta tavanının `afk_action =
leave_room` altında geride bıraktığı park: B40'ın `release_actions`'ı
parkın canlı bağlantıyla paylaştığı kanalı bırakırken içindekileri
saymıyordu (boşta taraması READ'den önce koşar; tavandan hemen önce
gönderilen istek oradadır). Artık `release_actions` kanalı
`drop_unread_requests`'le bırakır ve sayıyı döndürür (oda + shard; kilit
`room::tests::idle::leave::unread`, `shard::tests::idle::leave::unread`
— önce kırmızı: park 0, despawn 1). Yüzey: `RoomSample`/`RoomReport::requests_dropped_unread`,
`gsb-metric` satırında `req_unread=` (`req_refused=`'den sonra),
Prometheus/OTLP'de `gsb_room_requests_dropped_unread_total`, loadgen
metrik telinde `GSMH` (GSMG + `requests_refused_congested`'ten hemen
sonra alan; SUM ile katlanır), `RESULT`'ta `req_unread=` (her `req_*`
anahtarı gibi her satırda, mod olsun olmasın). İstemci teli değişmedi.

**Neden 2 — loadgen odanın sayaçlarını koşunun sonundan önce okuyordu
(ölçüm düzeneği).** Oda sayaçlarını metrik periyodu başına bir kez
örnekler (sunulan odalarda 1 Hz) ve dururken örnek göndermez; toplayıcı
kapanışta son raporunu elindeki örneklerden verir. Süreç içi koşu son
istemciden sonra yalnız 150 ms bekleyip sunucuyu durduruyordu: `RESULT`'un
`req_*`'ı odanın son PERİYODİK örneğiydi — bitişten bir periyoda kadar
önce. O pencerede okunan istekler `req_ext`'te yoktu; boşluk bitişin son
örneğin ne kadar ötesine düştüğüyle büyüyordu. Tam sayılı `--duration`'da
bitiş örnek anına denk düştüğünden çoğunlukla görünmüyordu; kesirli
sürede her seferinde: düzeltmeden önceki ikiliyle 200 istemci, R=10,
`--duration` 8,005 / 8,012 / 8,020 / 8,028 sn → boşluk **0 / 11 / 14 /
34**, yalnız Düzeltme 1 ile **8 / 19 / 36 / 92** (`req_unread` her birinde
0 — ayrılışlar son örnekten sonraydı, görünmüyordu); `--duration 2,5`'lik
4 istemcilik uçtan uca test 97 isteğin 77'sini görüyordu.

**Düzeltme 2 (loadgen).** Son istemciden sonra bekleme = ayrılışların
oturması (150 ms) + bir tam oda metrik periyodu
(`RoomConfig::default().metrics_cadence_hz`; `run.rs::final_sample_grace`):
her oda ayrılışlarından sonra bir kez daha örnekler. Süreç içi her koşu
~1 sn uzar. Kilit: `loadgen_rpc.rs::the_rooms_ledger_covers_the_end_of_the_run`
(`--duration 2.5`; önce kırmızı: 77 ≠ 97). `--serve`/orkestre yolu RPC
modunu zaten reddediyor; oradaki sunucu çocuğunun son raporu bu turda
değişmedi. *(F35: bekleme yeniden 150 ms'lik ayrılış oturmasına indi —
`run.rs::LEAVE_SETTLE`. Periyot beklemesi, odanın son sayısı son rapora
yetişmediği için gerekiyordu; artık toplayıcının son raporu her odanın
`RoomFinal`'ını — ve her bağlantının son flush'ını — bekliyor (DESIGN
§12 "Son rapor üreticileri bekler"), defter duruşun kendi raporundan
kapanıyor. Kilit aynı test.)*

İkisiyle oda defteri kapanıyor:

> `rpc_sent = req_local + req_ext + Σ req_rej_* + req_refused + req_unread
> + req_unbound + requests_dropped_closed + requests_dropped_full
> + requests_no_room + requests_unprocessed
> + transport_udp_requests_dropped_full
> + transport_stream_requests_dropped_closed
> + transport_udp_requests_dropped_closed`

Her istek tam olarak BİR terimdedir: oda kovaları (işlendi, reddedildi,
yanıtsız reddedildi, okunmadı, bağlanmamış bağlantıdan çekildi) ile
bağlantı tarafındaki dört kenar (kapalı kanal, dolu kanal, oda yok,
sunucunun bitirdiği oturumun işlenmemiş kutusu) ayrık yerlerde ve ayrık
koşullarda sayılır — bir istek bağlantıda düştüyse odaya hiç
ulaşmamıştır. Son terim TAŞIMANINDIR (B58): rUDP demux'ı gelen kareyi
oturumun dolu kutusuna koyamayınca düşürür; güvenilir bantta gelmişse
onu ZATEN ACK'lemiştir, istemci yeniden göndermez ve istek hiç
yanıtlanmaz. Kare çözülmüş olduğundan (opcode bilinir) sınıflanabilir:
yalnız RPC istekleri bu terime girer (`conn::FrameKind`), oyun ve
kontrol kareleri kendi taşıma sayaçlarına. Bağlantı aktörü onu hiç
görmediğinden bağlantının hiçbir terimiyle çakışmaz. Akış kapılarında
(TCP/TLS/WS/QUIC) okuyucu kutuya bekleyerek gönderir, dolu kutuda
düşürmez; ama sunucu oturumu bitirip kutuyu kapattığında (B60) okuyucunun
o an elinde tuttuğu kare reddedilir — bağlantı başına en çok bir kare.
İstekse `transport_stream_requests_dropped_closed` (B66, sayım turu 4),
defterin ikinci taşıma terimi: aktör onu hiç almadığından
`requests_unprocessed` ile çakışmaz. rUDP'de karşılığı
`transport_udp_requests_dropped_closed` (B66): demux'ın kapalı kutuya
çözdüğü istek (`Closed` kolu; oturum silinir, sıradaki güvenilir kareler
de aynı yolda). Henüz ACK'lenmemişti ama yeniden göndereceği oturum yok;
ardından gelen datagramlar `udp_datagrams_no_session`'a düşer (datagram
birimi, defter terimi değil).

`loadgen_rpc.rs`'in uçtan uca testleri bu eşitliği doğrudan iddia ediyor
(makul hızda `req_ext + req_unread = sent`, cap patlamasında
`req_ext + req_rej_conn + req_unread = sent`, kesirli sürede aynı; sayım
turu 2'den beri her testte defterin BÜTÜN terimlerinin toplamı da:
`LEDGER`).

**B54: odanın işlemeden düşürdüğü girdi, iki yerde, türüne göre.**
(1) Oturum bitince kanalda okunmamış kalan DÜZ oyun girdileri
(`drop_unread` ve parkın `release_actions`'ı — B36'nın yerleri) artık
`actions_dropped_unread`'de sayılıyor (istekler eskisi gibi
`requests_dropped_unread`'de; ayrık). (2) READ'in bağlama çevirisi
(faz 1.5, oda + shard) bağlama satırı olmayan bağlantının (bayat oturum —
yapısal olarak nadir: eski kanal yeniden bağlamada ölür) çekilen
girdisini yalnız debug log'la atıyordu; artık türüne göre sayıyor: RPC
isteği `requests_dropped_unbound` (defterin terimi, `req_unbound=`),
düz girdi `actions_dropped_unbound`. Yüzey: oda satırında
`actions_unread=` / `actions_unbound=` (`team_expired=`'den sonra) ve
`req_unbound=` (`req_unread=`'den sonra); Prometheus/OTLP'de
`gsb_room_actions_dropped_{unread,unbound}_total`,
`gsb_room_requests_dropped_unbound_total`; loadgen telinde `GSML`;
`RESULT`'ta aynı anahtarlar (her satırda). Kilit:
`room::tests::unread` (+ `::unbound`), `shard::tests::unread` (+
`::unbound`), `…::idle::leave::unread`. Düz girdi için "yanıt borcu yok,
defter yok" gerekçesi (aşağıda elenen 4) sayılıp sayılmamasını değil,
hangi defterde olduğunu belirler: kayıp kayıptır, sayılır; istek
defterine karışmaz.

Son terim oda sayacı değil, net kapsamının (B51, aşağıda): üyeliği ODA
bitirdiğinde (atma, girdi-boşta tavanı, oda kapanışı/emekliliği)
bağlantının bildirimden önce kapalı kanala ilettiği istek odaya hiç
ulaşmaz. Loadgen'de üyeliği hep istemci bitirdiğinden 0 (testler bunu
da iddia ediyor).

**B55: defterin ayrılamayan iki kenarı ayrıldı.** Sayım turu 2'ye dek
iki kenar defterin DIŞINDAYDI: dolu action kanalında düşen istek
`actions_dropped`'ta oyun girdileriyle karışıktı, odası olmayan
bağlantıya gelen istek yalnız yarış sınıfı ihlal olarak `violations`'ta
sayılıyordu. Artık bağlantı aktörü ikisini de kendi sayacında sayar
(`ConnSample`/`NetReport::requests_dropped_full`, `::requests_no_room`;
`gsb-metric scope=net`'te `requests_dropped_closed=`'dan sonra;
Prometheus'ta `gsb_net_requests_dropped_full_total`,
`gsb_net_requests_no_room_total`; loadgen telinde `GSMM`; `RESULT`'ta).
`actions_dropped` bununla anlamca DARALDI: yalnız oyun-bandı girdisi
(adının dediği; HELP'i de bunu söylüyor). İhlal muhasebesi değişmedi:
odası olmayan istek hâlâ `violations`'ta ve bütçede; `requests_no_room`
onun defterdeki tek sayımıdır (ilk birkaçı `ERROR 6` alır, sonrakiler
sessiz — ikisi de sayılır). Kimlik doğrulamadan önce gelen istek de aynı
yoldan geçer (odası yok) ve aynı terimde sayılır. Kilit:
`gsb-core/tests/conn_counts/requests.rs` (dolu kanal: iki istek + bir
girdi → 2 ve 1; oda yok: katılmadan önce ve ayrıldıktan sonra → 2, ihlal
3 değişmeden), `room_close::forward_closed` (kapalı kanaldan sonraki
`ERROR 6`'lı istek → `requests_no_room` 1), `loadgen_rpc.rs` her testte
defterin 14 teriminin toplamını `rpc_sent`'e eşitler.

**B60: sunucunun bitirdiği oturumun işlenmemiş kutusu (sayım turu 3).**
Sunucu oturumu kendisi bitirdiğinde (pompanın/registry'nin hükmü,
odanın atması ya da boşta kapanışı, `RoomGone`, `Shutdown`, ihlal
bütçesi, ölü çıkış yolu) bağlantı aktörü döngüden çıkar; okuyucunun
hükmün arkasına koyduğu kareler — içlerindeki RPC istekleri dahil —
hiç işlenmiyor, yanıtlanmıyor ve hiçbir terimde sayılmıyordu; pre-auth
bütçesini aşan kare de (işlenmez). Artık aktör çıkarken kutuyu kapatıp
(`close()`, boşaltma kapasiteyle sınırlı) kalanları türüne göre sayar
(`conn::FrameKind`): RPC isteği `requests_unprocessed` (defterin
terimi), oyun bandı `actions_unprocessed`, diğer temel bant
`control_frames_unprocessed`; ölü çıkış yolunun hüküm taraması
(`adopt_pending_close`) gördüğü kareleri artık atmıyor, sayıyor; bütçeyi
aşan kare aynı sayaçlarda. Yüzey: `gsb-metric scope=net`'te
`close_notices_dropped=`'dan sonra; Prometheus/OTLP'de
`gsb_net_{requests,actions,control_frames}_unprocessed_total`; loadgen
telinde `GSMQ`; `RESULT`'ta, her satırda. `loadgen_rpc`'nin `LEDGER`'ı
artık 15 terim; loadgen'de hep 0 (oturumu hep istemci bitirir). Kilit:
`gsb-core/tests/conn_counts/unprocessed.rs` (hüküm/atma/akış reddi/durma,
oda yok oldu, ihlal bütçesi, pre-auth bütçesi, ölü çıkış yolu — her biri
kutuda bir istek + bir girdi + bir heartbeat bırakır; istemci-tarafı son
hiçbir şey bırakmaz); mutasyonlar (boşaltmayı kaldırmak, taramada atmak,
aşan kareyi saymamak, isteği yanlış sınıflamak) testleri düşürür.

**B51: üyelik bittikten sonra bağlantıda düşen istek (kapandı).**
`forward_to_room`'un `Closed` kolu artık sayar: `RPC_REQ` ise
`requests_dropped_closed`, oyun-bandı girdisiyse `actions_dropped_closed`
(`ConnSample` → `NetReport`; `gsb-metric scope=net` satırında
`input_rate_limited=`'den sonra, Prometheus'ta
`gsb_net_{actions,requests}_dropped_closed_total`, OTLP'de `_total`'sız,
loadgen telinde `GSMJ`, `RESULT`'ta `actions_dropped_top=`'tan sonra).
Düz girdiler de sayılıyor — B36'nın `req_unread` için verdiği "düz
action'ları sayma" kararının aksine: burada aynı `try_send`'in `Full`
kolu girdiyi zaten sayıyordu (`actions_dropped`), `Closed` kolu saymıyordu;
aynı kaybın bir kolda sayılıp ötekinde sayılmaması "her şeyi saymalıyız"
ilkesiyle çelişir ve maliyet kayıp yolunda bir tamsayı artışı. İki sayaç
ayrık (istek yalnız ikincisinde), böylece defter isteği girdiden ayırır.
Kapalı iletim bağlantıyı ayırdığından (sonraki kare `ERROR 6`, yarış
sınıfı ihlal olarak `violations`'ta) biten üyelik başına en çok bir kare
buraya düşer. Kilit:
`room_close.rs::forward_closed` — test registry'yi kendisi oynar,
bağlantıyı elindeki bir action kanalıyla oturtur, kanalı kapatır (atma /
boşta tavanı despawn'ı gibi) ya da düşürür (oda kapanışı), bildirimi
geç teslim eder; önce kırmızı: iki testte de sayaç 0, beklenen 1.

**Elenenler.** (1) *READ'i CONTROL'den önce koşmak ya da ayrılışı kanal
boşalana dek ertelemek:* faz sırası bir katılmanın kanalının kaydını ve
ayrılışın tek tick'te bitişini taşıyor; ertelenen ayrılış, ayrılmış
oyuncunun isteğini işler (ekonomi alımı gibi yan etki) ve yanıtın
gideceği oturum yoktur. (2) *Kalan istekleri işleyip yanıtı atmak:* aynı
yan etki; üstüne "yanıtlandı" sayılıp hiç teslim edilmeyen yanıtlar.
(3) *Var olan bir kovaya katmak:* `req_refused` tıkalı bağlantının cap'teki
yanıtsız retidir (F15'in ayrı tuttuğu anlam), `req_late` worker
raporudur (istek değil), `gsb_net_actions_dropped_total` bağlantının
kendi dolu kanalının girişte düşürdüğüdür (oda hiç görmez). (4) *Düz
action'ları da saymak:* ateşle-unut girdinin yanıt borcu yok, hiçbir
defter onları uzlaştırmıyor — B36'da kapsam dışıydı; sayım turu 2'de
(B54) ayrı bir sayaçta sayılıyor (yukarıda) — istek defterine karışmadan. (5) *Odanın dururken son bir
örnek göndermesi (Neden 2 için motor düzeltmesi):* oda ile toplayıcı
aynı ticker kapanışında biter — geç örnek toplayıcının son raporunu
kaçırabilir, yok edilmiş odanın örneği akümülatörde odayı diriltebilir;
ölçüm düzeneğinin kendi beklemesi daha dar ve yeterli. (Sayım turu 3'te,
B62, duran oda son örneğini ayrı bir olayla — `MetricsEvent::RoomFinal`
— veriyor: diriltmez, yok edilen odanın bekleme penceresine girer ya da
onu başlatır; dolu kanalda düşmez, yalnız toplayıcı gitmişse kaybolur.
Duruş, elde kalan okunmamış istekleri `req_unread`'e, borçlu yanıtları
ve uçuştaki istekleri B53'ün iki sayacına katar. Loadgen'in beklemesi
yerinde kalır: süreç içi koşuda sunucuyu durdurmadan önce odalar zaten
son periyodik örneklerini vermiş olur. F35'te yarış kökünden kapandı:
toplayıcının son raporu odaların son örneklerini bekliyor; loadgen'in
periyot beklemesi kalktı.)

**Ölçüm** (release, süreç içi, TCP, 32 çekirdek; B37 sonrası; parantezde
1 dk yük ortalaması). `sent = req_ext + req_refused + req_unread` her
satırda birebir; hepsinde `left = N`, `errors = 0`, dup/eşleşmeyen 0.

| Koşu | sent | req_ext | req_refused | req_unread | client_to | open | dropped |
|---|---|---|---|---|---|---|---|
| 200, R=10, `--duration 8` | 15 220 | 15 220 | 0 | 0 | 0 | 56 | 0 |
| 200, R=10, 8,005 sn | 15 263 | 15 263 | 0 | 0 | 0 | 0 | 0 |
| 200, R=10, 8,012 sn | 15 251 | 15 241 | 0 | 10 | 0 | 10 | 0 |
| 200, R=10, 8,020 sn | 15 273 | 15 271 | 0 | 2 | 0 | 8 | 0 |
| 200, R=10, 8,028 sn | 15 360 | 15 273 | 0 | 87 | 0 | 87 | 0 |
| 200, R=10, duraklama 3000/6000, 30 sn (4,3) | 58 052 | 52 182 | 5 870 | 0 | 5 133 | 832 | 25 842 |
| 500, R=10, duraklama 3000/6000, 30 sn (3,5) | 142 642 | 105 558 | 37 084 | 0 | 29 767 | 8 010 | 139 621 |

Karşılaştırma için düzeltmeden önceki ikiliyle §8.2'nin 200'lük uzun
duraklama koşusunun tekrarı (yük 3,8): 57 942 = 54 303 + 3 639 (boşluk
bu koşuda 0), yalnız Düzeltme 1'li ikiliyle (yük 20,0): 57 971 = 54 022
+ 3 917 + **32**. Kesirli sürelerde `open` = `req_unread` çıktı:
istemcinin "bitişte uçuşta" saydığı isteklerin bir kısmı oda hiç
okumamış isteklerdi. Uzun duraklama satırları §8.2'dekilerle aynı
şekli taşıyor (B37 sonrası; sayılar koşudan koşuya oynuyor).

## 9. Kontrol düzlemi: oda yaşam döngüsü ve maç-sonucu dikişi

**API (composition root):** `ServerHandle::open_room(RoomConfig)`,
`close_room(RoomId)`, `room_status(RoomId)` — hepsi registry
round-trip'idir. **Registry oda ASLA bekleyemez:** durum kendi tablosundan
cevaplanır (member sayısı, her odanın join/leave raporlarından
registry'nin tuttuğu bağlantı tablosunun taramasıdır — oda turu yok).

- **Idempotent açma:** var olan oda + **aynı** `RoomConfig` (tam
  eşitlik) → `Ok(Running{members})`, **fabrika çağrılmaz** ("şu odayı
  aç" iki kez → **bir** oda; yeniden gönderilen istek odayı ikiye
  katlayamaz). **Farklı** config → `Err(RoomConflict(id))` (typo
  koruması: config değişikliği bir **karar**dır, yan etki değil —
  sessizce oda değiştirmek yerine kontrol düzlemi hata görür).
- **Idempotent kapama:** yok oda → `Ok(Absent)` (retry eden kontrol
  düzlemi ikinci kapamada hata GÖRMEZ).
- **Durum:** `Running{members}` / `Destroyed` (ara pencere: registry
  kaydı hemen silinir, oda **bir sonraki tick'inde** durur) /
  `Absent`.
- **Maç sonucu:** `RoomLogic::match_result(&mut self, world) ->
  Option<Bytes>` — `on_shutdown()`'dan SONRA, dünya **hâlâ canlıyken**
  çağrılır (logic ECS sorgusu yapabilir; demo, son dünya snapshot'ını
  rapor eder). Sonuç, sınırlı bir `result_sink` mailbox'ına
  **best-effort `try_send`** ile gider: oda sink'e asla blok etmez
  (dolmuş/olmayan sink = atılan sonuç — dikişin sözleşmesi
  "oda kapanırken son durumu bir kez dışarıya iletme", garanti değil
  kuyruk). Atılan sonuç sayılır (B57): toplayıcıya
  `MetricsEvent::MatchResultDropped` ile, registry diliminde
  `gsb_registry_match_results_dropped_{full,closed}_total`. Referans adapter composition root'un `ServerHandle
  .match_results` alıcısını okumasıdır (in-proc, tek hop — base'de
  NATS/Kafka/gRPC **yok**).
- **Referans adapter (dış I/O):** `gsb_game::economy::EconomyService` —
  mailbox + tek görev, yapılandırılabilir gecikme (varsayılan 5 ms;
  tick periyodunun altı seçilmiştir: worker bir tick içinde çözülür,
  yanıt yine de **bir sonraki** tick'te çıkar), `PRICES` tablosu.
  Platform bunu gerçek bir servis ile (DB/HTTP/imza) değiştirir;
  odanın `External` sözleşmesi aynı kalır.

## 10. Tel (wire) formatı ekleri

- **Base band:** `RPC_REQ = 12` (istemci→sunucu), zarf
  `RpcRequest { id: u64, op: u32, payload }`. Proto3'te 16-bit tamsayı
  yok; op uzayı protokol sözleşmesiyle **u16**'dır — sınırda cast
  edilir (zarf `u32` taşır).
- **Sunucu→istemci:** `Private`'a `repeated RpcResponse responses = 3`
  eklendi — oneof'un **DIŞINDA** (tel uyumlu: eski istemci alanı
  yok sayar; `Payload::Ack` kodu değişmedi).
  `RpcResponse { id, ok, op, reason, payload }`.
- **Auth:** `Auth.ticket` (bytes, boş = local auth); `AuthResult`'a
  `player` ve `room` (kancanın kimliği + sabitlediği oda).
- **Hata kodları:** **10** (bilet hatası/timeout/eksik — normal ret),
  **11** (join ≠ biletin odası — normal ret).
- **Sharded odalar:** bu turda RPC'sizdiler; sonrasında **kapatıldı** —
  shard actor'ü odanın makinesinin tam karşılığını taşıyor (pending set +
  cap'ler + timeout sweep + completion uzlaşması; `TRAIT-ARCHITECTURE.md`
  Faz 3). Bağlantı-anahtarlı pending, resume semantiğini (§11) korur;
  match_result shard başına bir payload üretir (adaptör birleştirir).

## 11. Bu turda YAPILMAYANLAR (NOT-DONE)

- **Sharded oda RPC'si:** shard tick gövdesinde istek fazı yoktu
  (shard'lar `ShardLogic` üzerinden; RPC deseni tek-oda actor
  topolojisi için tasarlanmıştı) — **sonradan kapatıldı**: yukarıdaki
  §10 notu ve `TRAIT-ARCHITECTURE.md` Faz 3; shard süit kilidi
  `crates/gsb-core/tests/rpc_shard.rs`.
- ~~**Gerçek bilet doğrulayıcıları**~~ **Yapıldı (B21):** opt-in
  `gsb-ticket` (PASETO v4.public, oyunun taleplerine jenerik, oyun
  kontrolü, tek kullanım bekçisi, `[ticket]` config'i); base yine biçim
  bilmez — docs/TICKETS.md.
- ~~**NATS/Kafka/gRPC adapter'ları**~~ **Kararla kapsam dışı (B22,
  2026-10-03):** dış servis adaptörü oyunun işi; motor
  `RequestDecision::External` ve `ServerHandle::match_results`
  dikişlerini verir — tarif docs/TICKETS.md §13.
- **Bağlantı başına RPC geçmişi:** yanıtlanan id anında yeniden
  kullanılabilir (sınırsız seen-set yok).
- **Sabitleme sonrası oda taşınması:** biletin odası, bağlantının
  ömrü boyunca sabittir (yeniden auth gerekir).
- **Worker havuzu (hazır task kümesine devir):** oda başına sabit K
  worker + job kuyruğu tasarımı tartışıldı ve ertelendi — task sayısını
  O(uçuşta) yerine O(K)'ya, backend eşzamanlılığını K'da sabitlerdi;
  2000'lik mevcut cap'te per-request spawn güvenli (1–4 MB, ~2–4 ms
  uyanma yayılımı). Havuz, 10 000 üyeli oda ölçeğinde ve/veya ret
  kovaları oda cap'inin fiilen bağlıyor olduğunu gösterdiğinde bir sonraki
  adım (kuyruk gecikmesi = overload'da dürüst timeout).
- **Conn-side gate:** per-connection in-flight kümesinin conn actor'üde
  tutulması (cap + dup kaynakta enforce) — bağımsız takip adımı; oda
  tarafı cap bu turda yerinde kaldı.
- ~~**Loadgen'de RPC trafiği**~~ **Yapıldı (B23):** `--rpc-rate R
  [--rpc-burst B]` — mod, defter, RESULT anahtarları ve ölçüm §8.2'de.
  Kalan: mod yalnız demo'nun `ECONOMY`'sini (dış-I/O yolu) gönderiyor —
  oda-local `ABILITY` yolu yük altında ölçülmedi (menzil kontrolü
  istemcinin kendi konumunu bilmesini ister); orkestre / churn koşuları
  modu reddediyor (CLIENT satırı defteri taşımıyor).
- ~~**Oda üyeliği bitirdikten sonra bağlantı actor'ünde düşen istek (B36
  kalıntısı)**~~ **Yapıldı (B51):** kapalı kanala iletilen istek
  bağlantı aktöründe `requests_dropped_closed` olarak (düz girdi
  `actions_dropped_closed` olarak) sayılıyor, defterin son terimi — §8.3.
- ~~**Odası olmayan bağlantının isteği**~~ **Yapıldı (B55):**
  `ERROR 6` + `violations` aynen; ayrıca `requests_no_room`'da defterin
  terimi (§8.3). İstemci bunu hâlâ korelasyonlu bir yanıt olarak görmez
  (defteri onu açık/yanıtsız sayar) — tel değişmedi.
- ~~**Dolu action kanalında düşen istek**~~ **Yapıldı (B55):**
  `requests_dropped_full`; `actions_dropped` artık yalnız oyun girdisi.
- ~~**Sunucunun bitirdiği oturumun kutusunda kalan istek**~~ **Yapıldı
  (B60):** `requests_unprocessed` (pre-auth bütçesini aşan kare dahil),
  defterin terimi — §8.3.
- ~~**rUDP demux'ının dolu kutuda düşürdüğü istek**~~ **Yapıldı (B58):**
  `transport_udp_requests_dropped_full` (taşıma kapsamı), defterin
  taşıma terimi — §8.3.
- ~~**Akış okuyucusunun kapalı kutuya veremediği istek**~~ **Yapıldı
  (B66):** `transport_stream_requests_dropped_closed`, defterin terimi —
  §8.3.
- ~~**rUDP demux'ının kapalı kutuya çözdüğü istek**~~ **Yapıldı (B66):**
  `transport_udp_requests_dropped_closed`, defterin terimi — §8.3.

## 12. Testler: sözleşmenin kilidi

| Sözleşme | Test |
|---|---|
| Oda-local istek aynı tick'te, yalnız o bağlantıya (sızma yok) | `gsb-core/tests/rpc.rs::local_request_answered_same_tick_only_own_conn` |
| Dış I/O tick'i bloklamaz, yanıt sonraki tick'te | `rpc.rs::external_request_does_not_block_tick_answers_later_tick` |
| Timeout: tam olarak bir yanıt (süpürge + koruma) | `rpc.rs::timeout_swept_exactly_one_answer` |
| Bağlantı açık istekle kapatıldı → slot boşalır, geç rapor atılır | `rpc.rs::conn_close_in_flight_frees_slots_and_drops_late_report` |
| Çalışan id dup'ı reddedilir (her tür için), sonra yeniden kullanılabilir | `rpc.rs::duplicate_inflight_id_rejected_then_reusable` |
| Bağlantı/oda cap'leri | `rpc.rs::per_conn_pending_cap`, `control_plane.rs::create_conflict_different_config` |
| Aynı tick'te action-önce-istek sırası | `rpc.rs::actions_before_requests_same_tick` |
| Düşen batch'in yanıtları sonraki kabul edilen batch'le, tam bir kez ve sırayla (oda + shard) | `gsb-core/src/room/tests/fanout/replies.rs::a_dropped_answer_rides_the_next_accepted_batch_once`, `shard/tests/replies.rs::a_dropped_answer_rides_the_next_accepted_batch_on_the_shard` |
| Düşmesiz tick'te/teslim edilmiş yanıt tekrar gitmez | `fanout/replies.rs::a_delivered_answer_is_not_resent_by_a_later_drop` (+ shard) |
| Fırtına sınırı: tıkalı bağlantı cap kadar borçlanır; sınır cap + bir tick'in çekimi; uçuştaki istek borca dahil; tıkalı olmayan bağlantı reddedilmez | `fanout/replies/bound.rs::a_congested_connection_owes_at_most_its_in_flight_cap`, `::the_storm_bound_is_the_cap_plus_one_ticks_pull`, `::in_flight_requests_count_toward_what_is_owed`, `shard/tests/replies/bound.rs::a_congested_connection_owes_at_most_its_cap_on_the_shard`, `::an_uncongested_connection_is_never_refused_on_the_shard` |
| Ayrılan/resume eden oturum teslim edilmemiş yanıtları götürür (sızıntı yok) | `fanout/replies/session.rs::a_leaving_connection_takes_its_undelivered_answers_along`, `::a_resumed_session_does_not_inherit_undelivered_answers` (+ shard leave) |
| Kontrol düzlemi: idempotent açma (tek oda), çakışma, durum yaşam döngüsü | `control_plane.rs::*` |
| Maç sonucu kapanışta dışarı (yeniden oluşturulabilir oda ikinci sonucu verir) | `control_plane.rs::match_result_reports_on_destroy` |
| Bilet: geçerli/hatalı/boş/geç doğrulama; oda sabitlemesi; bütçe etkileşimi | `ticket.rs::*` |
| Her AUTH kararı tek sebeple sayılır (oyun adı ayrı; delta iki kez sayılmaz) — B21 | `gsb-core/tests/conn_counts/tickets.rs` |
| Doğrulanmış talepler yönlendiriciye ve katılım kancasına ulaşır — B21 | `gsb-core/tests/join_identity/claims.rs`, `gsb-kit/src/game/tests/claims.rs` |
| PASETO v4 resmi vektörleri; her doğrulama kuralı; uçtan uca lobi → izin → mühürlü rUDP — B21 | `gsb-ticket/src/{paseto,validator}/tests.rs`, `examples/lobby/tests/lobby.rs` |
| Ret nedenlerinin metni sabit (oda + shard + loadgen aynı sabitleri okur; baytlar aynı) | `gsb-core/src/rpc.rs::tests::the_rejection_reasons_are_pinned` |
| Loadgen RPC defteri: ilk yanıt kapatır; ikinci yanıt yalnız dup; gönderilmemiş id yalnız eşleşmeyen; sınır geç / yanıtsız / açık'ı ayırır; çekirdek nedenleri sınıflanır | `gsb-server/src/loadgen/client/rpc/ledger/tests.rs` (6 test) |
| Ayrılışta okunmamış istekler sayılır (tam bir kez, yalnız istekler); defter kapanır (oda + shard; ayrılış, despawn eden kopuş, yeniden katılım, resume, ölü göç) | `gsb-core/tests/rpc/unread.rs::requests_unread_when_the_leave_lands_are_counted`, `rpc_shard/unread.rs::*`, `src/room/tests/unread.rs::*`, `src/shard/tests/unread.rs::*` |
| Üyeliği oda bitirdikten sonra bildirimden önce iletilen istek / girdi bağlantıda sayılır (tam bir kez, ayrık; sonraki kare `ERROR 6`) — B51 | `gsb-core/tests/room_close/forward_closed.rs::*` |
| Loadgen RPC modu uçtan uca: makul hızda her istek bir kez `ok`, istemci zaman aşımı 0; cap'in üstündeki patlamada istemci ve oda aynı cap ret sayısını görür; oda defteri kapanır (`req_unread` dahil, B36); modsuz satırda `rpc_*` yok | `gsb-server/tests/loadgen_rpc.rs` |
| Tel üzerinden: idempotent yaşam döngüsü, runtime oda dolu (kod 8), maç sonucu, RPC sızma-yok/sonraki-tick, bilet akışı + yavaş-auth penceresinde tick canlılığı | `gsb-server/tests/e2e.rs` (son beş test) |
