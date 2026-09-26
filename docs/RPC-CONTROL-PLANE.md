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
  süpürme.
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
`req_refused`, `req_to`, `req_late`): aynı yolun iki ucu yan yana.
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
   hiçbir oda sayacına düşmedi (500'lük koşuda 0): büyük olasılıkla
   ayrılıştan hemen önce gönderilip oda ayrılışı işlerken atılan istekler
   — oturum kapsamlı durum (§3.1), ama hiçbir yerde sayılmıyor; küçük bir
   görünürlük boşluğu, mekanizması doğrulanmadı.
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
`--workers 1` veriyor (`args.workers.max(1)`; yorum "runtime default"
diyor). Düzeltilmedi — iki ayrı karar: kapalı kanalı `dropped`'tan
ayırmak bir metrik anlamı değişikliği, orkestratörün worker sayısını
değiştirmek geçmiş orkestre tabanlarının koşulunu değiştirir.

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
  kuyruk). Referans adapter composition root'un `ServerHandle
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
- **Gerçek bilet doğrulayıcıları:** base yalnız kancayı ve e2e demo
  doğrulayıcılarını taşır (platform davranışı platformdadır).
- **NATS/Kafka/gRPC adapter'ları:** yalnız in-proc referans
  (ekonomi + result sink alıcısı).
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
| Ret nedenlerinin metni sabit (oda + shard + loadgen aynı sabitleri okur; baytlar aynı) | `gsb-core/src/rpc.rs::tests::the_rejection_reasons_are_pinned` |
| Loadgen RPC defteri: ilk yanıt kapatır; ikinci yanıt yalnız dup; gönderilmemiş id yalnız eşleşmeyen; sınır geç / yanıtsız / açık'ı ayırır; çekirdek nedenleri sınıflanır | `gsb-server/src/loadgen/client/rpc/ledger/tests.rs` (6 test) |
| Loadgen RPC modu uçtan uca: makul hızda her istek bir kez `ok`, istemci zaman aşımı 0; cap'in üstündeki patlamada istemci ve oda aynı cap ret sayısını görür; modsuz satırda `rpc_*` yok | `gsb-server/tests/loadgen_rpc.rs` |
| Tel üzerinden: idempotent yaşam döngüsü, runtime oda dolu (kod 8), maç sonucu, RPC sızma-yok/sonraki-tick, bilet akışı + yavaş-auth penceresinde tick canlılığı | `gsb-server/tests/e2e.rs` (son beş test) |
