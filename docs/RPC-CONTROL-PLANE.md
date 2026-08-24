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
  (env + yanıt kuyruğu), oda başına 2000 `PendingRequest` + 2000 worker
  görevi + 2000 slot'lu `completions` kanalı (varsayılanlar; §6'daki
  türetim). Cap aşımı = aynı tick'te normal ret (§6).
- **Ret sayaçları nedene göredir** (bu turda tek `requests_rejected`
  yerini altı kovaya bıraktı — `req_rej_malformed / _dup / _no_handler
  / _logic / _conn / _room`): her kova farklı bir operasyonel soruya
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
  deneyebilir.
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
- **Loadgen'de RPC trafiği:** smoke assert'leri sıfır-trafik invariant'larını
  doğruluyor; yük altında ret kovalarını gözlemleyecek RPC üreten bir
  loadgen modu yok.

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
| Kontrol düzlemi: idempotent açma (tek oda), çakışma, durum yaşam döngüsü | `control_plane.rs::*` |
| Maç sonucu kapanışta dışarı (yeniden oluşturulabilir oda ikinci sonucu verir) | `control_plane.rs::match_result_reports_on_destroy` |
| Bilet: geçerli/hatalı/boş/geç doğrulama; oda sabitlemesi; bütçe etkileşimi | `ticket.rs::*` |
| Tel üzerinden: idempotent yaşam döngüsü, runtime oda dolu (kod 8), maç sonucu, RPC sızma-yok/sonraki-tick, bilet akışı + yavaş-auth penceresinde tick canlılığı | `gsb-server/tests/e2e.rs` (son beş test) |
