# Değişim Günlüğü (CHANGELOG)

Tamamlanan geliştirme turlarının donmuş, tarih-sıralı kaydı (en yeni
üstte). Açık işler ve öncelikler: `docs/ROADMAP.md`. Tasarım
dokümanları: DESIGN / CROSS-SHARD / DISTRIBUTED / SECURITY / OPS /
TRAIT-ARCHITECTURE / RECONNECT.

## F41 — duruşta önce kapılar (`core/f41-stop-order`)

- **Hata (motor, gerçek):** `stop()` registry'ye `Shutdown`'ı gönderip
  kapıları ANCAK sonra kapatıyordu. Registry `Shutdown`'da okumayı
  bırakır: arkasına kuyruklanan `ConnOpened` posta kutusuyla düşer,
  sonrakinin gönderimi hata alır; accept döngüsü aktörü yine doğurur. O
  aktör `ConnIn::Shutdown` almaz, ERROR 14 göndermez, eşi kapatana ya da
  idle penceresine (30 sn) dek yaşar ve son raporu `FINAL_REPORT_GRACE`'e
  dek tutar (`final_report_complete=false`). İki pencere: accept ile
  `ConnOpened` gönderimi arası (dolu registry kutusunda genişler) ve
  kapının kapandığı yoklamada biten accept (`Door::admit` elindeki
  bağlantıya meyillidir). Kanıt: eski sırayla yeni test deterministik
  kırmızı — registry `[Closed(BUSY), Shutdown, Opened(c1)]` okudu.
- **Düzeltme (`boot/stop.rs`, yalnız sıra):** kapılar (ops + her listener)
  kapanır → `end_accepts` (1 sn tek son tarih) → registry `Shutdown` →
  ticker abort → servisler → toplayıcı. Döngü yalnız "kapandı" hatasında
  döner; gönderdiği her `ConnOpened` `Shutdown`'ın önündedir, her aktör
  kayıtlı ve bildirimli (ERROR 14) biter. Abort edilen döngü aktör
  doğurmaz. Kapıdan sonra gelen eş kabul edilmez; kapının düşürdüğü zaten
  B74 sayaçlarında — yeni sayaç yok.
- **Sınırlar:** `stop()`'un üst sınırı aynı (≈ 3 sn); toplayıcının sınırı
  artık accept beklemesinden sonra başlar. S kuralı değişmedi. İstemci
  teli değişmedi.
- **Elenenler:** reddedilen `ConnOpened`'da aktörü doğurmamak (kuyrukta
  düşen `ConnOpened`'ın gönderimi başarılı döner); "kapat, `Shutdown`,
  sonra bekle" (aynı yoklamada biten accept kaçar); registry'nin
  `Shutdown`'dan sonra boşaltmaya devam etmesi (kutunun EOF'u gelmez).

Testler 1455 → 1458 (`otlp` ile 1473 → 1476): `boot::stop::tests::window`
(3; paused saat, gerçek accept döngüsü + bağlantı aktörü + toplayıcı +
`stop()`, sahte kapı ve dolu kutulu sahte registry); eski sırayla ikisi
düşer, 200/200 yeşil. Ebeveyn doğrulaması: dinleyicileri kapatmamak iki
testi düşürdü. DESIGN §9 "Önce kapılar (BACKLOG F41)", §12.

## F35 — son raporun odaları beklemesi (+F33, F40) (`metrics/f35-final-report`)

- **Hata (motor):** toplayıcı son raporunu ticker kapanışında basıyordu;
  odalar (`on_shutdown` → maç sonucu → sayım → `RoomFinal`) ve bağlantı
  aktörleri (registry'nin `Shutdown`'ı → son flush) sonlarına ANCAK o an
  başlıyordu. İlk periyodik örneğine (30 adım) varmamış oda rapordan
  bütünüyle eksik kalıyor, her oda son örneğinden sonra saydıklarını
  kaybediyordu. Kanıt: `loadgen_churn_smoke`'un komutu iş parçacığı
  düzeyinde aç bırakmada 40 koşunun 20'sinde `steps=0 room_resumes=0`,
  "server metrics: unavailable"; smoke'un komutu %90 donmada 8/8 oda
  satırsız (F33, aynı kök).
- **Düzeltme:** kapanıştan sonra toplayıcı olay kanalını bekler, her olayı
  katlar, kanal KAPANINCA (her gönderici düştü) son raporu basar;
  `post`'un doğurduğu gönderici kendi klonunu tuttuğundan kapanış onu
  geçemez. Sınır `FINAL_REPORT_GRACE` = 2 sn (kapanıştan, `stop()`'un
  kendi sınırlarıyla yan yana — `stop()`'un üst sınırı uzamadı); aşılırsa
  rapor sınırda çıkar, `warn` + `StopReport::final_report_complete =
  false` (`MetricsCollector::run` artık `bool` döner). Taşıma görevleri
  ayrı bir kanala (`with_transport_events`) gönderir: katlanır ama
  kapanışı beklenmez (sessiz eşin pump'ı duruşu aşabilir).
- **Loadgen:** `final_sample_grace` (150 ms + bir metrik periyodu, B36) →
  `LEAVE_SETTLE` 150 ms; odanın sayıları duruşun son raporundan.
- Sonra: aç bırakmada 0/40 (`steps=17–39`), %90 donmada 0/8.
- **F40 (loadgen):** `--serve --metrics-listen` çocuğu akışına kimse
  bağlanmazsa çıkmıyordu (`metrics_export` `accept`'te). Ana görev
  `stop()`'tan sonra dışa aktarımı `EXPORT_STOP_GRACE` = 2 sn bekler,
  aşarsa keser (stderr satırı, çıkış 0).
- Elenenler: ticker'ı odaların bariyerinden sonra kesmek (bağlantıları
  kapsamaz); `stop()`'un bariyerden sonra "son" işareti göndermesi (dolu
  kanaldaki `post`'u sıralamaz); tek kanal + sınır (sessiz eşli her duruş
  sınırı bekler); kör beklemeyi uzatmak.
- **API:** `StopReport::final_report_complete` alanı eklendi (struct
  literal ile `StopReport` kuranı kırar); `metrics::FINAL_REPORT_GRACE`,
  `MetricsCollector::{with_transport_events, with_final_grace}`.
- RESULT, `/metrics`, istemci teli değişmedi. CONTRIBUTING: odanın
  kendisini aç bırakmak için sürecin iş parçacıklarının HEPSİ itilir.
- **Ebeveyn doğrulaması — test boşluğu kapatıldı:** son oturum olayından
  sonra gelen bir taşıma sayımının kanal kapanışındaki son boşaltmayla
  alınması hiçbir testte sınanmıyordu (boşaltmayı kaldıran mutasyon tam
  paketten sağ çıktı); `a_transport_word_after_the_last_session_event_is_in_the_final_report`
  eklendi — gerçek kodda geçer, mutasyonda düşer.
- Yan bulgu **F41** (koddan): `stop()` kapıları kapatmadan registry'ye
  `Shutdown` gönderiyor; aradaki bağlantı duruşu aşabilir.

Testler 1447 → 1455 (`otlp` ile 1465 → 1473): `metrics::tests::final_report`
(4, paused saat), `service_stop::the_final_report_carries_every_rooms_final_count`
(eski davranışla 5/5 kırmızı), `loadgen_smoke::a_run_shorter_than_a_metrics_period_still_reports_the_room`
(10/10 kırmızı), `accept_stop::a_silent_peer_does_not_hold_the_final_report`,
`loadgen_serve::a_served_server_nobody_reads_still_exits` (önce 21 sn
korumasına takıldı). DESIGN §9/§12 "Son rapor üreticileri bekler (F35)",
RPC-CONTROL-PLANE §8.2/§8.3, GAME-MODULE "Raporlama".

## F32 — kopuşu geçen yeniden bağlanma (`reconnect/f32-churn`)

`loadgen_churn_smoke` 128 `yes` altında bir kez `resumed=0` ile düşmüştü
(dört istemcinin dördü ikinci oturumda taze varlık). Katman: **motor** —
meşru bir resume iki sıralamada kayboluyordu:

- **(A)** Registry yeni join'i eski bağlantının kapanışından önce görür
  (yarı açık soket ya da kuyruktaki `ConnClosed`): "iki canlı oturum"
  sayıp eskisine düz LEAVE yolluyordu → varlık `on_leave` ile gider, yeni
  oturum taze join alır — oyuncu kopuşu sunucudan önce fark edip dönerse
  karakterini kaybediyordu.
- **(B)** Registry kapanışı önce görür ama eski dağıtıcının DETACH'ı yeni
  dağıtıcının resume'undan sonra odaya varır: resume parkı bulamaz, ikinci
  varlığı taze açar; geç DETACH ilkini kimsenin resume edemeyeceği yetim
  bir park olarak bırakır (bir kimliğe iki varlık).

Kanıt: doğal koşularda pay 0,6 ms'ye kadar indi (registry `ConnClosed`'u
aynı kimliğin `SpawnPlayer`'ından 0,6 ms önce işledi); sıra elle
çevrilince (300 ms gecikme) smoke'un komutu 10/10 tam olarak `resumed=0
fresh_joins=4` verdi, düzeltmeden sonra 0/10. Süreç dondurma (F25
yöntemi) görevler arası sırayı koruduğu için bu yarışı üretmez.

- **Düzeltme (oda + shard):** kimliği başka bir bağlantıda CANLI bulan
  resume önce o oturumun ayrılmasını (`detach_player`, `ConnectionClosed`,
  politikaya sorularak) çalıştırır, sonra parkı alır (`room::live_session`).
  Eski DETACH geç gelirse bağlamayı taşınmış bulur, no-op.
- **Düzeltme (registry, `players/handover.rs`):** canlı eski oturuma
  ERROR 9 (Superseded) aynen; LEAVE yerine üyelik DEVREDİLİR (satır odadan
  çıkar, grid sayacı bir düşer, yeni `SpawnDone` geri sayar; `reg_leaves`
  sayılmaz).
- **Davranış değişikliği (sözleşme):** aynı kimlikle ikinci bir oturum
  açıldığında eski oturum yine ERROR 9 ile kapanır, ama yeni oturum artık
  taze varlık değil ESKİ varlığı alır (oyunun kopuş politikası park
  ediyorsa; `grace = 0` politikasında taze join, doğru sonuç). Eski
  davranış varlığı `on_leave` ile yok ediyordu. §12.7 ve
  `broadcast_resume_accepted_by_exactly_one_shard` testleri yeni
  sözleşmeye göre sıkılaştı. RECONNECT §3.3/§5 "Kopuşu geçen yeniden
  bağlanma (F32)"/§11.
- Elenenler: resume'u eski DETACH'a bariyerle bekletmek ((A)'yı kapatmaz;
  B61 yolunu kilitleyebilir); loadgen'i "sunucu fark edene dek"
  bekletmek (istemcinin göreceği işaret yok, gerçek istemci tam bu
  sırayla döner); yeni `DisconnectCause::Superseded`.
- CONTRIBUTING "Gerçek saatli testler"e iş parçacığı düzeyinde aç bırakma
  ve sırayı elle çevirme yöntemi eklendi.
- Aç bırakmada ayrı bir flake kaldı: `room_resumes=0` (odanın son örneği
  raporda yok) → F35.

Testler 1442 → 1447 (`otlp` ile 1460 → 1465): `room/tests/takeover.rs`
(2), `shard/tests/takeover.rs`, `tests/reconnect/takeover.rs` (2); eski
kodda altı test düştü; yedi mutasyon öldü. Ebeveyn doğrulaması: anonim
kimlik korumasını kaldırmak eşdeğer (boş kimlik hiçbir yoldan resume
olarak gelmiyor — dağıtıcı ve oda önce ayırıyor; koruma savunma
derinliği).

## F31 — sunucu çocuğunun port yarışı (`loadgen/f31-port`)

- **Hata:** orkestratör sunucu çocuğunun oyun ve metrik portlarını
  kendisi seçiyordu (`alloc_port`: `127.0.0.1:0`'a bağla, numarayı oku,
  kapat, çocuğa ver); aradaki boşlukta portu başkası alırsa çocuk
  "Address already in use" ile ölüyordu. Kanıt: 128 `yes` + `:0`'a
  sürekli bağlanan 4 port kapıcı altında eski ikili 20 koşunun 18'inde
  temiz bitmedi.
- **Düzeltme:** çocuk iki kapısını da `:0`'a bağlar ve bağladığı adresleri
  stdout'a tek satırla bildirir: `SERVING addr=… metrics=…`
  (`serve/announce.rs`; metrik dinleyicisi önce, sonra sunucunun
  dinleyicileri). Orkestratör satırı en çok 30 sn bekler
  (`procs/server_child.rs`), çocuğun öteki stdout satırlarını aynen
  geçirir, istemcileri bildirilen adrese yollar, metrik akışına bir kez
  bağlanır (yeniden deneme döngüsü kalktı). Satırdan önce ölen, susan
  (öldürülüp biçilir) ya da bozuk satır basan çocukta koşu orada biter:
  çıkış 1, hata çocuğun çıkışını adlandırır, istemci çocuğu başlatılmaz.
  `alloc_port` silindi. Açık port veren komut satırları aynen çalışır.
  RESULT ve istemci teli değişmedi.
- Elenenler: hazır dosyası (tekil yol + temizlik), adresi metrik akışının
  el sıkışmasında taşımak (oyun adresi o bağlantıdan önce lazım),
  ayırıcıyı tutup bind hatasında yeniden denemek (yarış kalır). Aynı kalıp
  başka yerde yok (testler `:0` + `handle.addr` kullanıyor).
- Sonra: aynı yük altında 20/20 temiz, 10/10 ayrı süreçli smoke,
  SIGSTOP/SIGCONT 250/250 ms'de 8/8. Yan bulgu **F40** (metrik akışına
  kimse bağlanmazsa `--serve` çocuğu çıkmıyor).

Testler 1431 → 1442 (`otlp` ile 1449 → 1460): `announce` (2),
`server_child/tests.rs` (7; sahte çocuk `/bin/sh`), `child_args` (1; önce
kırmızı), `tests/loadgen_serve.rs` (1; önce kırmızı); 7 mutasyonun hepsi
öldü. Ebeveyn doğrulaması: bildirim satırında metrik adresini hep `-`
basmak testi düşürdü. RPC-CONTROL-PLANE §8.2 "Sunucu çocuğunun portları
(F31)".

## Sayım turu 7 — B82 (doğruluk hatası), B83 (`core/b82-late-resume`)

- **B82 — yavaş shard'a resume (gerçek doğruluk hatası):** sharded resume
  yayını cevap başına 5 sn bekliyordu; sınır dolunca katlama hepsi-ıska
  sayıp ev shard'ına taze join yapıyordu, oysa yavaş shard'ın kutusunda
  resume hâlâ kuyruktaydı. Park o shard'daysa sonra kabul edip park
  satırını bağlantının `out`'una bağlıyordu; geç kabul aktarıcı görevde
  sayılmadan düşüyordu. Kanıt (iki gerçek shard aktörü): bağlantı iki
  shard'a bağlı, istemci iki snapshot akışı alıyor, girdisi geri bağlanan
  satıra hiç ulaşmıyor; park shard'ındaki satır registry'nin izlemediği
  canlı bir üye — kapanışın `Detach`'i ev varlığını taşıdığından satır oda
  ömrünce yaşar (kapalı kanala yayın, yuva ve dünya varlığı tutulur);
  girdi-boşta tavanı onu ikinci kez park edebilir ya da canlı üyeliği
  bitirebilir. Duraklama gerekmiyordu: `tick_hz` 0,2'nin altındaki bir
  sharded odada her resume tetikleyebilirdi. Ayrıca zaman aşımına uğrayan
  tur n turdan birini yiyor, zamanında gelen bir cevap okunmadan
  kalabiliyordu.
- **Karar:** sınır yok — dağıtıcı her canlı shard'ın cevabını bekler (düz
  join tek shard'ını zaten öyle bekliyordu); shard'lar paralel cevap
  verdiği için bekleme en yavaşınınki, bir kez. Giden shard'lar yine
  hiçbir şeye mal olmaz (B71). Toplama kanalı ve aktarıcı görevler kalktı;
  geç cevap kalmadığından sayılacak kayıp da yok. Kabul edilen: asılı
  (adım atmayan) bir shard o odaya resume eden bağlantıyı da bekletir —
  düz join zaten bekletiyordu. Elenenler: geç kabulü `Detach` ile geri
  almak (ikinci park ya da canlı üyeliği silen `DetachDespawned`),
  resume'a son tarih/jeton koymak (yarış kalır, beklemeyi kısaltmaz),
  sınırda istemciye hata (hayalet satır yine oluşur).
- **B83 — WS okuyucusunun kapalı kuyruğu:** `queue_control` yalnız dolu
  kuyruğu sayıyordu; soket yazıcısı başarısız yazmayla durduktan sonra
  pong/kapanış yankısı sessizce düşüyordu. Artık
  `ws_close_frames_dropped_closed` / `ws_pongs_dropped_closed`
  (`gsb_transport_*_total`). Loadgen metrik teli **GSNA**.
- Ajanın taraması: bunlardan sonra motorda sayılmayan kayıp kalmadı —
  yalnız zaten gitmiş bir şeye giden (içerik taşımayan) bildirimler ve
  belgelenmiş gerçekten sayılamayan uçlar (süreç inerken toplayıcı yok,
  B70, 1 sn `settle`). B81 demo kodu.

Testler 1427 → 1431 (`otlp` ile 1445 → 1449):
`registry/actor/dispatch/tests/late.rs` (3; eski kodda üçü de düştü: 5.
saniyede ev shard'ına taze join), `ws/tests/lost_controls.rs`; mutasyonlar
öldü. Ebeveyn doğrulaması: yalnız ilk cevabı beklemek dört testi düşürdü.
RECONNECT §6 "Yavaş shard'a resume (B82)".

## F25 — yükte riskli gerçek saatli testler (`test/f25-realtime`)

Sayım turu 5'in `otlp` koşusunda `loadgen_smoke` yük ortalaması 31–40
iken 1,61 Hz / 0 Hz raporlayıp düşmüştü. Meşgul döngüler bunu üretmiyor
(128 `yes` altında loadgen 30 Hz); üreten, bütün sürecin donması (swap
baskısı): test ikilisini SIGSTOP/SIGCONT ile periyodik durdurmak
(400/100 ms'de iki demo smoke'u 10/10 düştü). Listedeki testler aynı
yöntemle (250/250 ms) önce/sonra 10'ar kez koşturuldu.

- **loadgen smoke'ları** (`loadgen_smoke`, `_separate_processes`,
  `loadgen_games`, `loadgen_ws`): 3 sn'lik duvar penceresinden "~30 Hz"
  (20–40 Hz, ≥ 60 adım, ≥ 30 snapshot/istemci) bir makine performansı
  iddiasıydı — ticker takılmada patlamaz, saate oturur. Yeni
  `tests/loadgen_rate` yalnız takılmanın değiştiremeyeceğini iddia eder:
  odanın kendi örneğindeki `budget_us` = 1/30 sn (yapılandırma odaya
  ulaştı), `0 < steps ≤ 30 × T + 2` (T = sürecin ömrü; çift tick atan
  oda yakalanır, 15 Hz'e daraltınca düşer), iki hız sayı, medyan istemci
  ≥ 1 snapshot. Hızın kendisi paused saatte sabit
  (`room::tests::paused_clock`: 2 sn'de 60 adım); gerçek makinenin 30 Hz'e
  ulaşması elle ölçüm koşularının işi. Donma altında 0/10 → 10/10.
- **`boot::stop`** paused saatte (`took == grace`); yeni test:
  `end_accepts` son döngü bitince döner. **`accept_stop`** 900 ms yerine
  el sıkışma süresiyle (10 sn) sınırlı. 9/10 → 10/10.
- **RPC zaman aşımı süpürmesi** paused saatte: tikler süreden tam 30 ms
  önce/sonra (79 ms sessiz, 81 ms cevaplı).
- **udp busy-band** kesin: her okuma yeniden gönderir
  (`retrans_out == okuma`).
- **`tests/input_rate`**: 1,1 sn uykular yerine gönderilen her kareyi
  saymış ilk rapor. 2/10 → 10/10.
- **e2e ticket**: yavaş doğrulayıcı serbest bırakılana dek park eder;
  A'nın snapshot'ları heartbeat ACK çitinin arkasında sayılır (daha güçlü:
  eski pencere kuyruktakileri de sayıyordu). 1/10 → 10/10.
- **slow_reader'lar** (TCP/QUIC/WS): pencere 300 ms → 1 sn, çerçeveler
  "birkaç pencere"yi yapıdan sağlar. 0/10 → 10/10.
- **Hata (ayrı commit): orkestratörün metrik okuyucusu** sunucu çocuğu
  metrik akışı sunmadan ölürse sonsuza dek bağlanmayı deniyordu (yük
  altında 51 dk asılma: çocuk `alloc_port` ile bind arasında portu
  kaptırdı). Çocuk bittikten sonra 5 sn süre, sonra uyarı ve boş sunucu
  sayıları. Kilit testi düzeltmeden önce 90 sn korumasına takıldı;
  ebeveyn doğrulaması: süreyi fiilen sınırsız yapmak testi düşürdü.
- CONTRIBUTING "Gerçek saatli testler" F25 örnekleriyle genişledi.
- Kalanlar BACKLOG F30–F34 (port yarışı F31 dahil).

Testler 1425 → 1427 (`otlp` ile 1443 → 1445).

## Sayım turu 6 — "her şeyi saymalıyız": B75, B80 (`metrics/count-everything-6`)

- **B75 — duran odanın reddettiği katılmalar:** dağıtıcı `RoomGone`'un iki
  nedenini ayırır (`OpOutcome`): kutu gönderimi REDDETTİ (`Refused` —
  oda/shard durmuş ya da ölmüş, op'u hiç görmedi) ya da op'u alıp cevabı
  DÜŞÜRDÜ (`Gone` — oda duruşta `joins_unprocessed`/`resumes_unprocessed`
  sayar). Reddi dağıtıcı sayar: `MetricsEvent::JoinRefusedClosed` doğrudan
  toplayıcıya (registry üzerinden değil — bütün sunucunun duruşunda
  registry önce çıkar) → `joins_refused_closed=`,
  `gsb_registry_joins_refused_closed_total`. Kimliği KENDİ parkında olan
  duran shard kuyruktaki resume'u sayar ve `Err(RoomGone)` der ("burada
  sayıldı"): katlama taze join'e düşmez, çift sayım yok. Parkı hiçbir
  yerde olmayan resume taze join'e düşer ve orada bir kez sayılır.
  Elenenler: bütün kuyruktaki resume'lara `RoomGone` (parksız resume'u
  kimse saymazdı); sayımı `SpawnFailed` ile registry'de tutmak. Loadgen
  teli **GSMY**.
- **B80 — WS'nin 1001'i:** `poll_close` kapanış çerçevesini `try_send` ile
  koyuyordu; dolu kuyrukta (son batch yavaş okuyana gidiyor) sayılmadan
  düşüyor, istemci kapanış görmüyordu. **Karar: teslim** — kapanış oyun
  karesi gibi slot bekler (`ws/writer/going_away.rs`), pompa kapanışı
  yazma-tıkanma penceresi altında bekler; aynı bayt (`88 02 03 E9`),
  yalnız kaybolmadan. Teslim edilemeyen: `ws_going_away_unsent_closed`
  (yazıcı başarısız yazmayla durmuş) ve `ws_going_away_unsent_stalled`
  (pencere doldu). Okuyucunun kendi kapanışı kazanır, sayılmaz. Yan
  düzeltme: soket yazıcısının erken çıkış boşaltması `recv` ile bekler —
  kapanıştan önce slot ayırmış göndericinin karesi de sayılır. Loadgen
  teli **GSMZ**.
- Altın metinler yalnız yeni aileler kadar değişti; `otlp::cross` yeşil.
  İstemci teli değişmedi.
- Kalan adaylar: **B82** (resume yayınının 5 sn sınırından sonraki geç
  cevabı — geç bir KABUL bağlantıya iki akış verebilir; doğruluk sorunu
  olabilir), B83 (WS okuyucusunun kapalı kuyruğa veremediği pong/yankı).

Testler 1412 → 1425 (`otlp` ile 1430 → 1443); her madde önce düştü,
mutasyonla doğrulandı. Ebeveyn doğrulaması: alınıp düşürülen cevabı da
ret saymak iki testi düşürdü. Not: yük altında `loadgen_ws` bir süre düştü
(F25'e eklendi).

## Sayım turu 5 — "her şeyi saymalıyız": B72–B74 (`metrics/count-everything-5`)

- **B72 — takım hub'ının reddedilen röleleri:** hub (registry'nin içinde)
  hedef shard'a kuyruklayamadığı import'u yalnız `team_hub_summary` log
  penceresinde, DOLU ve KAPALI karışık (`relay_drops`) sayıyordu. Artık
  sebebe göre ayrı ve metrik yolunda: `on_export` export'un reddettiklerini
  döndürür, registry kümülatif toplamı tutar (hub odayla gider) ve ret
  olduğunda örneğini hemen gönderir (röle tablo değiştirmez). Registry
  satırında `team_relays_dropped_{full,closed}=`,
  `gsb_registry_team_relays_dropped_{full,closed}_total`. Loadgen teli
  **GSMW**.
- **B73 — `udp_frames_drained` daraldı:** `batch.len()` sayıyordu, aynı
  kanaldan gelen demux ACK taşıması (`UDP_ACK`) da giriyordu. Artık yalnız
  oturumun kareleri (oyun + kontrol), `udp_frames_unsent` gibi; ad korundu,
  HELP değişti.
- **B74 — kapanan kapı ve rUDP accept tarafı:** el sıkışan kapının
  (WS/TLS/QUIC) `close`'unun kestiği el sıkışmalar
  (`handshakes_cut_closed`) ve kuyrukta attığı bitmiş uç noktalar
  (`handshakes_unaccepted_closed`) sayılır; intake'in son örneği kapının
  oturmasını bekler (`intake/close.rs`, `settle`, 1 sn sınırlı). rUDP:
  accept tarafı gitmişken doğrulanan oturum
  (`udp_sessions_dropped_accept_gone`) ve kabulü gitmiş ama kuyruktayken
  dinleyici kapanan oturum (`udp_sessions_unaccepted_closed`; kuyruk öğesi
  `Queued` düşerken kendini sayar). Loadgen teli **GSMX**.
- Altın metinler yalnız yeni aileler ve bir HELP kadar değişti;
  `otlp::cross` yeşil. İstemci teli değişmedi.
- Ajanın değerlendirmesi: B75 ve B80 ile belgelenmiş, gerçekten
  sayılamayan uçlar (süreç inerken toplayıcı gitmiş, panikleyen görevin
  içeriği — B70 —, 1 sn `settle` zaman aşımı) dışında motorun "her kaybı
  say" taraması tamam. B81 yalnız demo.

Testler 1405 → 1412 (`otlp` ile 1423 → 1430); her madde önce düştü,
mutasyonla doğrulandı. Ebeveyn doğrulaması: kapalı ret sayısını dolu
sayaca katmak testi düşürdü. Not: otlp koşusunda yük altında
`loadgen_smoke` bir kez düştü (F25'e eklendi).

## B71 — duran sharded odada resume hemen yanıtlanıyor (`core/b71-resume-stop`)

- **Hata (gecikme):** `dispatch_resume` shard cevaplarını toplama kanalına
  aktaran görevlerle bekliyor, kanalın kendi göndericisini (`agg_tx`)
  elinde tutuyordu. Cevapsız giden shard (duranın `finish`'i kuyruktaki
  cevapları düşürür, ölen shard'ın kutusu göreviyle gider, kapalı kutu
  gönderimi reddeder) kanalı kapatamıyor, her eksik cevap 5 sn'lik
  sınırın tamamını yiyordu: duran odaya resume `RoomGone`'unu shard başına
  5 sn geç alıyordu (üç shard: 15 sn).
- **Düzeltme:** dağıtıcı yayından sonra göndericisini bırakır; her shard
  cevap verdiğinde ya da gittiği bilindiğinde katlama biter. Cevap aynı
  bayt (`RoomGone`), yalnız erken; canlı ama yavaş shard için sınır aynen.
  Elenen: `finish`'in kuyruktaki resume'lara `RoomGone` demesi (ölen
  shard'ı, reddedilen gönderimi ve açık tutulan kanalı çözmez).
- **Sayım B75'e:** parkı hiçbir shard'da olmayan kuyruktaki resume ve
  dağıtıcının `RoomGone` ile yanıtladığı katılmalar hâlâ sayılmıyor;
  mevcut hiçbir sayaç kesin anlamla taşıyamıyor, yeni registry sayacı
  gerekiyor.

Testler 1403 → 1405 (`otlp` ile 1421 → 1423):
`registry/actor/dispatch/tests.rs` (duraklatılmış saat; önce düştü: 15 sn,
10 sn); mutasyonlar öldü. Ebeveyn doğrulaması: göndericiyi bırakmamak iki
testi düşürdü. İstemci teli, altın metinler ve aileler değişmedi.
RECONNECT §6 "Duran odada resume (B71)".

## Sayım turu 4 — "her şeyi saymalıyız": B66–B68 (`metrics/count-everything-4`)

- **B66 — akış pompalarının kayıpları:** yazıcı pompası başarısız yazma ya
  da yazma tıkanmasıyla bitince yazdığı batch'in kalanını ve çıkış
  kanalında duran batch'leri sayar (`stream_frames_unwritten`,
  `stream_batches_unwritten` — oda/bağlantı onları "gönderildi"
  saymıştı: yeni sayaç "kanal aldı" ile "sokete ulaştı" arasındaki fark).
  WS soket yazıcısı kuyruğunu (kontrol `ws_control_frames_unwritten`) ve
  kapanıştan sonraki oyun karelerini (`ws_frames_dropped_after_close`)
  sayar. Okuyucunun kapalı kutuya veremediği kare türüne göre
  (`stream_{requests,actions,control_frames}_dropped_closed`). Düz TCP de
  (`spawn_pumps`/`TcpTransport` `metrics` alır).
- **B66 — rUDP:** yazıcının soketin reddettiği datagramları banda göre,
  bant ölünce gönderilmeyen kareler (`udp_frames_unsent`), demux'ın
  reddedilen ACK/challenge gönderimleri, kapalı kutuya çözülen kare
  (`Closed` kolu, türüne göre) ve oturumsuz adresten gelen datagram.
  **`die`'ın yanlış atfı düzeltildi:** `RelDead` bildirimi artık akış
  pompasının ayrılmış slotuyla gider (`pump::verdict`; oturum başına bir
  posta kutusu slotu) — dolu kutuda düşüp kapanış `outbound_dead` diye
  yanlış sayılmıyor; slot ayrılamayan nadir yol `writer_verdicts_deferred`.
  Elenen: `channel::post` (spawn'lı gönderim aktör kutusuna baktıktan sonra
  varabilir, atıf yine yanlış kalır).
- **B67 — panikle ölen oda/shard:** ölüm bekçisi `JoinHandle` hatasında
  görevin satır kimliğiyle `MetricsEvent::RoomEndedUncounted` gönderir;
  toplayıcı `rooms_ended_uncounted`'a sayar (registry dilimi; görev başına)
  ve satırın beklemesini başlatır — ölen shard'ın satırı artık budanıyor.
  **Yan bulgu (düzeltildi):** bir shard ölünce hayatta kalan shard'lar
  sunucu durana dek çalışıyor, `RoomGone` almış üyelere yayın yapıyordu;
  `on_room_died` artık `stop_room` çağırır, onlar kendi son sayımlarıyla
  biter.
- **B68 — duranın oturum dışı kalanları:** `finish` kanalı kapatıp
  kalanları sayar (`metrics::StopCounts`, dokuz sayaç, son örnekte):
  işlenmeyen join/resume/leave/detach (yayın op'ları yalnız etki edeceği
  shard'da), kurulmayan gelen göç (oyuncunun okunmamış girdisi dahil),
  gönderilmeyen/uygulanmayan etkiler, takım/border görünüm güncellemeleri.
  **Yan bulgu (düzeltildi):** shard'ın CONTROL fazı `Shutdown`'a
  rastlayınca boşaltmanın kalanını sayılmadan atıyordu.
- **RPC defteri:** `+ transport_stream_requests_dropped_closed +
  transport_udp_requests_dropped_closed` (13 terim).
- Altın metinler yalnız yeni aileler (ve oda satırının dokuz anahtarı)
  kadar değişti; `otlp::cross` yeşil. Loadgen teli **GSMV**. İstemci teli
  değişmedi.
- Kalan sayılmayanlar BACKLOG B70–B74; **B71 bir gecikme hatası** (duran
  sharded odada resume shard başına 5 sn bekliyor).

Testler 1381 → 1403 (`otlp` ile 1399 → 1421); her madde mutasyonla
doğrulandı. Ebeveyn doğrulaması: ölen shard'ın hayatta kalanlarını
durdurmamak testi düşürdü.

## B65 — geç gelen `OpsClosed` taze dağıtıcıyı silmiyor (`core/b65-opsclosed`)

- **Temizlik:** registry, dağıtıcının `OpsClosed`'unda bağlantının
  `conn_ops` girdisini kimin olduğuna bakmadan siliyordu. B63'ten beri
  girdi, gitmiş dağıtıcının yerini alan taze bir dağıtıcı olabilir; eski
  görevin geç raporu taze dağıtıcının tek göndericisini düşürür, kuyruğu
  kapanır ve dağıtıcı (B61 gereği) canlı bağlantının üyeliğini DETACH
  ederdi. Bugün erişilemez (dağıtıcı, göndericisi yuvada dururken yalnız
  panikle biter; panikleyen görev `OpsClosed` yollamaz).
- **Düzeltme:** her dağıtıcının bir seri numarası var — `install_conn_ops`
  registry'de basar, göndericinin yanında `conn_ops`'ta saklar, göreve
  verir; görev `OpsClosed { conn, serial }` yollar, `on_ops_closed` girdiyi
  yalnız numara tutarsa siler. Öteki silme/değiştirme yerleri denetlendi:
  hepsi bilerek o anki dağıtıcıyı bırakır ya da yalnız gitmiş bir
  göndericinin üstüne yazar.
- **Elenenler:** `Sender::same_channel` (mesajda gönderici taşımak kuyruğu
  açık tutar, B61'i bozar); girdiyi yalnız göndericisi kapalıysa silmek
  (kimliği görev içindeki bırakma sırasına emanet eder).

Testler 1379 → 1381 (`otlp` ile 1397 → 1399): `registry/actor/players/
tests/late.rs` (2); önce düştü, dört mutasyon öldü. Ebeveyn doğrulaması:
eşitlik yerine `>=` karşılaştırması testi düşürdü. Tel, `/metrics`,
goldenlar değişmedi. RECONNECT §3.4.

## Sayım turu 3 — "her şeyi saymalıyız": B58–B60, B62 (`metrics/count-everything-3`)

- **B59 — dolu kanalda düşen bağlantı örneği deltalarını kaybetmiyor:**
  bağlantı aktörü sayaç tabanını yalnız kanal örneği ALDIĞINDA ilerletir
  (`conn/actor/flush.rs`); düşen örneğin deltaları bir sonraki örnekte,
  düşüş `metrics_dropped`'ta. Ayrıca **son örnek** dolu kanalda artık
  düşmez: `channel::post` ile doğurulan gönderici bekler (aktör beklemez;
  önceden hüküm ve deltalar sayılmadan kayboluyordu — DESIGN §12'nin
  "kabul edilen maliyet"i kapandı).
- **B60 — sunucu kararlı sonun işlenmemiş kutusu:** aktör çıkarken gelen
  kutusunu kapatıp kalanları türüne göre sayar (`conn::FrameKind`); ölü
  çıkış yolunun hüküm taraması ve pre-auth bütçesini aşan kare de. Net
  kapsamında `requests_unprocessed` (RPC defterinin yeni terimi),
  `actions_unprocessed`, `control_frames_unprocessed`. `frames_in` anlamı
  değişmedi (kutuda kalanlar içinde yok, aşan kare var).
- **B62 — duran odanın son sayımı:** oda/shard `finish()`'te elde
  kalanları (okunmamış girdi — park dahil —, uçuştaki istekler, borçlu
  yanıtlar) oturum sonu sayaçlarına katar ve son örneği ayrı bir olayla
  verir (`MetricsEvent::RoomFinal`, `post` ile). B36'nın itirazlarına
  cevap: toplayıcı onu bekleme penceresinde de satıra alır, `RoomGone`
  yoksa pencereyi kendisi başlatır (hayalet yok); olay yalnız toplayıcı
  gitmişse (süreç inerken) kaybolur. `MatchResultDropped` da `post` ile.
  **Yan bulgu (düzeltildi):** yok edilen sharded odanın shard satırları
  hiç budanmıyordu (`room << 16 | index` ≠ `RoomGone`'un kimliği); artık
  her shard'ın `RoomFinal`'ı kendi satırını budar.
- **B58 — taşımanın kendi kayıpları:** yeni `transport` kapsamı
  (`TransportCounters`, 18 sayaç, `gsb_transport_*_total`,
  `gsb-metric scope=transport`, RESULT `transport_*=`). rUDP demux'ının
  dolu kutuda düşürdüğü kareler türüne göre (istek → RPC defterinin taşıma
  terimi `udp_requests_dropped_full`; kare o noktada çözülmüş olduğundan
  sınıflanabiliyor), demux'ın diğer düşüşleri, yazıcının parça
  tavanı/terk/boşaltma kayıpları, WS okuyucusunun dolu kontrol kuyruğunda
  düşen kapanış/pong'u, el sıkışan kapıların ret/zaman aşımı/başarısızlığı.
  Yol: taşıma görevi `gsb_net::metrics::Flusher` ile delta
  `MetricsEvent::Transport` gönderir (500 ms'de bir + bitişte; B59
  kuralı); `gsb-net` zaten `gsb-core`'a bağlı, katman ters dönmüyor.
  Elenenler: ortak atomikler, toplayıcının dinleyicileri okuması,
  bağlantı aktörü üzerinden raporlama.
- **RPC defteri:** `rpc_sent = req_local + req_ext + Σ req_rej_* +
  req_refused + req_unread + req_unbound + requests_dropped_closed +
  requests_dropped_full + requests_no_room + requests_unprocessed +
  transport_udp_requests_dropped_full` (`loadgen_rpc` 16 terimi iddia eder).
- Altın metinler yalnız yeni aileler, iki HELP (`requests_undelivered`,
  `requests_abandoned` odanın duruşunu da sayar) ve taşımanın
  `metrics_dropped` payı kadar değişti; `otlp::cross` yeşil. Loadgen metrik
  teli **GSMR**. İstemci teli değişmedi.
- Kalan sayılmayanlar BACKLOG B66–B69 (en büyüğü: yazıcı pompasının
  çıkışta kanalda bıraktığı batch'ler).

Testler 1353 → 1379 (`otlp` ile 1371 → 1397); her madde önce kırmızı ya da
mutasyonla doğrulandı. Ebeveyn doğrulaması: `RoomFinal`'ın bekleme
penceresini başlatmaması testi düşürdü.

## Küçük paket 8 — B63, B64 (`core/small-bundle-8`)

- **B63 — gitmiş dağıtıcı artık bağlantıyı kilitlemiyor.** Görevi gitmiş
  (kuyruğu kapanmış) op dağıtıcısının ölü göndericisi `conn_ops`'ta
  kalıyor, bağlantının sonraki her katılması `join_ops_dropped` ile
  reddediliyordu. `Join`'in `Closed` reddinde registry artık ölü
  göndericiyi taze bir dağıtıcıyla değiştirir (`respawn_conn_ops`) ve
  op'u ona bir kez verir; o da reddederse (yalnız runtime kapanırken)
  katılma eskisi gibi düşer ve sayılır. Tabloda hâlâ bir üyelik varsa
  (eski görevin kabul edip çalıştırmadığı bir ayrılma) taze görev o
  üyelikle başlar (`spawn_conn_ops`'un yeni `seed`'i) ve ilk op'u o
  ayrılmadır: oda ayrılmayı yeniden denenen katılmadan önce, tek
  görevden, sırayla görür. Elenen: `direct_leave` (spawn'lı gönderimi
  aynı odaya katılmayla yarışır, oda eski entity'nin ayrılmasını bayat
  diye yutar). Sınır: yalnız eski görevin bildiği (raporlanmamış) bir
  katılma kurtarılamaz (B61 ile aynı). Bugün dağıtıcıda panik yeri yok;
  gizli bir takılmaydı.
- **B64 — eşleşmeyen `Leave` üyeliği unutturmuyor.** Dağıtıcının `Leave`
  kolu önce odayı karşılaştırır; eşleşmeyen `Leave` bugünkü gibi yanıtsız
  kalır ve `in_room`'a dokunmaz. Bağlantı aktörü üzerinden erişilemez.
- Yan not BACKLOG B65: `OpsClosed` işleyicisi girdiyi gönderici kimliğine
  bakmadan siliyor (bugün erişilemez).

Testler 1350 → 1353 (`otlp` ile 1368 → 1371): `registry/actor/players/
tests.rs` (2), `registry/actor/conns/ops/tests.rs` (1); önce düştüler,
mutasyonlar öldü. Ebeveyn doğrulaması: taze dağıtıcıyı bekleyen üyelik
olmadan başlatmak testi düşürdü. RECONNECT §3.4.

## B61 — düşen `Close` üyeliği artık sızdırmıyor (`core/b61-close-op-leak`)

- **Hata:** kapanan bağlantının `RoomOp::Close`'u dağıtıcı kuyruğuna
  giremeyince (16'lık kuyruk dolu ya da görev gitmiş) dağıtıcı kuyruğunu
  boşaltıp detach'sız çıkıyordu: odadaki üye, registry satırı ve ızgaranın
  üye yuvası oda bitene dek kalıyor, oyunun `on_disconnect`'i hiç
  çalışmıyordu. Kuyruktaki tekrar katılmalar yüzünden kalan üyelik,
  tablonun henüz görmediği yeni bir entity'ydi. Bağlantı aktörü her
  katılmanın yanıtını beklediği için bugün tek bir gerçek bağlantı
  kuyruğu dolduramıyor — gizli bir sızıntıydı (ham `RegistryMsg`, ileride
  op'ları boru hattına dizen bir yol ya da bir panik ile erişilebilir).
- **Düzeltme:** dağıtıcı kuyruğunun kapanmasını `Close` sayar: önündeki
  op'lar sırayla çalıştıktan sonra elindeki üyeliği DETACH eder,
  `DetachDone`/`OpsClosed` yollar (kuyruğa girmiş `Close` ile aynı sıra).
  Görev gitmişse registry tablodaki üyeliği `send_detach_direct` ile
  (spawn'lı, S kuralı) kendisi detach eder. `on_disconnect` bir kez,
  `ConnectionClosed` ile çalışır. `close_ops_dropped` reddi saymayı
  sürdürür ("Close op kuyruğa girmedi"); aile, tel ve goldenlar değişmedi.
- **Elenen:** dolu kuyrukta tablodan doğrudan detach — bayat entity'yi
  hedefler, oda onu yutar, son üyelik yine sızar (mutasyonla gösterildi).
- Katılma yolu kontrol edildi: sızıntı yok (ızgaranın `pending` yuvası
  retde bırakılıyor). Yan bulgular BACKLOG B63 (gitmiş dağıtıcının ölü
  göndericisi), B64 (`Leave` kolunun sırası).

Testler 1347 → 1350 (`otlp` ile 1365 → 1368): `tests/room_close/close_op.rs`
(tek oda + ızgara) ve `registry/actor/conns/tests.rs` (gitmiş dağıtıcı);
önce düştü, üç mutasyon düşürür. Ebeveyn doğrulaması: reddi saymamak üç
testi düşürdü. RECONNECT §3.4.

## Sayım turu 2 — "her şeyi saymalıyız": B53–B57 (`metrics/count-everything-2`)

Bakımcı ilkesi ikinci turda: her kayıp, adı anlamıyla aynı bir sayaçta;
karışık anlamlı metrik yok, gerekirse yeni sayaç.

- **B53 — oturumu biten bağlantıya borçlu yanıtlar:**
  `drop_conn_request_state` (ayrılış, park, üst üste katılım, göç) ve
  BROADCAST sonu süpürme, attığı yanıtları `requests_undelivered`
  (`req_undelivered=`, `gsb_room_requests_undelivered_total`), attığı
  uçuştaki dış istekleri `requests_abandoned` (`req_abandoned=`) olarak
  sayar (oda + shard). Defter terimi değil — isteğin kendisi kendi
  kovasında; bunlar yanıtın akıbeti. Geri konan yanıt (F14) süpürmeden
  sonra döndüğü için bir kez sayılır.
- **B54 — odanın işlemeden düşürdüğü girdi:** oturum sonunda kanalda
  okunmamış DÜZ girdiler `actions_dropped_unread`; READ'in bağlanmamış
  bağlantının girdisini düşürmesi türüne göre `requests_dropped_unbound`
  (defter terimi) ve `actions_dropped_unbound`.
- **B55 — defterin karışık iki kenarı ayrıldı:** dolu aksiyon kanalında
  düşen RPC isteği `requests_dropped_full`; **`actions_dropped` artık yalnız
  oyun-bandı girdisi** (HELP'i de öyle). Odası olmayan bağlantının isteği
  `ERROR 6` + `violations` aynen, ayrıca `requests_no_room`. Defter:
  `rpc_sent = req_local + req_ext + Σ req_rej_* + req_refused + req_unread +
  req_unbound + requests_dropped_closed + requests_dropped_full +
  requests_no_room` — her istek tek terimde; `loadgen_rpc` her testte
  terimlerin toplamını iddia eder.
- **B56 — heartbeat kısmasının fazlası metrikte:**
  `heartbeats_throttled_{preauth,authed}` (önceden yalnız debug satırı).
- **B57 — kontrol düzlemi ve çıkış kayıpları:** `send_frame` kareyi
  gönderimden SONRA sayar (**`frames_out`/`bytes_out_control`/
  `bytes_out_total` artık yalnız kanalın aldıkları**); kapalı kanalın
  reddettiği kare `frames_out_closed`, dolu kanalda düşen kapanış
  bildirimi `close_notices_dropped`. Registry kapsamında
  `join_ops_dropped`, `close_ops_dropped`,
  `match_results_dropped_{full,closed}` (`registry::send_match_result`,
  `MetricsEvent::MatchResultDropped`). **`shipped_*`/`private_frames`/
  `bytes_out_room` artık yalnız kanalın aldığı batch** (düşen yük iki kez
  görünmüyor). Kapalı registry kutusunda düşen istekler bilerek sayılmıyor
  (yalnız süreç inerken).
- Altın metinler yalnız yeni aileler/anahtarlar ve
  `gsb_net_actions_dropped_total` HELP'i kadar değişti; `otlp::cross`
  yeşil. Loadgen metrik teli **GSMP**. İstemci teli değişmedi.
- Turun bulduğu açıklar BACKLOG B58–B62; **B61 gerçek bir sızıntı**
  (düşen `RoomOp::Close` oda satırını ve yuvasını oda bitene dek tutuyor).

Testler 1320 → 1347 (`otlp` ile 1338 → 1365): yeni
`gsb-core/tests/conn_counts` (gerçek bağlantı aktörü, registry'yi test
oynar), oda/shard undelivered/unbound/shipped birimleri,
`registry::counters::ops`, `control_plane::dropped`, loadgen tel testleri;
önce kırmızı, mutasyonlar öldü. Ebeveyn doğrulaması: kapalı kanalın
reddettiği kareyi yine `frames_out`'a saymak testi düşürdü.

## B52 — boş odanın kesiti nüfus değil (`loadgen/b52-mmo-flake`)

- **Belirti:** `loadgen_orchestrates_the_mmo` yükte (128 süreçlik CPU yükü
  altında 30 koşuda 5 kez) `shard_members=0,0,0,0`, `records_per_tick=0.0`
  basıyordu.
- **Mekanizma (rapor akışıyla doğrulandı):** toplayıcı, shard'ların
  örneklediği ticker'ın aynısında ve aynı saniyelik periyotla yayıyor;
  yükte yayını onların turunu bölüyor. Oyuncuların içeride olduğu 2–3
  raporun hepsi yırtık ya da eksik çıkabiliyor; tek tutarlı kesitler
  istemciler ayrıldıktan sonra geliyor (orkestre sunucu çocuğu 3 sn fazla
  koşar) ve kimseyi tutmuyor. `has_consistent_cut` bu boş kesitlerle doğru
  döndüğü için nüfus yalnız onlardan okunuyordu: tepe 0, kararlı pencere
  boş kesitler.
- **Düzeltme loadgen'in seçiminde** (`report/spread.rs`): nüfus tutarlı
  kesitlerden ancak içlerinden biri oyuncu tutuyorsa okunur
  (`has_populated_cut`). Kesitleri hep boş oda olan koşu, kesitsiz koşu
  gibi yırtık geri düşüşle okunur ve insan-okunur blok `(torn: …)` der.
  RESULT biçimi, `/metrics` ve tel baytları değişmedi.
- Elenenler: sunucu çocuğuna B36 tarzı son örnek beklemesi (sorun son
  rapor değil, oyunculu raporlar), birden çok rapordan kesit kurmak (bazı
  satırlar hiç yayılmıyor), toplayıcı yayınını kaydırmak (çekirdek
  değişikliği — BACKLOG F29), testi uzatmak/gevşetmek.
- Kalan risk: yırtık geri düşüş göç anında bir oyuncuyu ±1 okuyabilir
  (tasarımda yazılı; blok söyler).

Testler 1318 → 1320 (`otlp` ile 1336 → 1338): `report::spread::tests::empty`
(B52 akışını satır satır oynatır; önce kırmızı, tepe 0) + F18 koruması;
mutasyonlar öldü. Gerçek test yük altında önce 37/40, sonra 60/60.
Ebeveyn doğrulaması: koşulu tersine çevirmek dört testi düşürdü.

## Sayım turu — "her şeyi saymalıyız": B32, B51 ve geride bırakılan parkın kanalı (`metrics/count-everything`)

Bakımcı kararı (2026-09-27, B32 seçenek a): her kayıp, anlamı adıyla
aynı bir sayaçta sayılır; karışık anlamlı metrik yok.

- **B32 — kapalı kanal `dropped` değil:** fan-out'un bağlantı başı
  `try_send`'i iki türlü düşer ve artık ayrı sayılır (`SendFailures`; oda
  ve shard BROADCAST'i aynı yardımcıyı kullanır; RPC yanıtları ve private
  kareler aynı batch'te). **Full** (yavaş istemci) `dropped`'ta kalır —
  `gsb_room_dropped_total` HELP'inin dediği. **Closed** (bağlantı gitmiş,
  oda sonunu henüz işlememiş; istemcinin istediği kare kaybolmaz) yeni
  `sends_closed`'a gider: `gsb-metric` `sends_closed=`, Prometheus/OTLP
  `gsb_room_sends_closed_total`, RESULT `sends_closed=`. Oran göstergesi
  yok (bağlantı sonlarıyla sınırlı). Orkestre 500'ün 116'sı bugün
  `sends_closed`'a düşer.
- **B51 — üyelik bittikten sonra bağlantıda düşen iletim:** oda üyeliği
  kendisi bitirdiğinde (atma, girdi-boşta tavanı, oda kapanışı) bağlantının
  bildirimden önce kapalı kanala ilettiği kare `forward_to_room`'un
  `Closed` kolunda sayılıyor: `RPC_REQ` → `requests_dropped_closed`,
  oyun-bandı girdisi → `actions_dropped_closed` (aynı `try_send`'in `Full`
  kolu düz girdiyi zaten sayıyordu). Net kapsamı: `gsb-metric scope=net`,
  Prometheus/OTLP `gsb_net_{actions,requests}_dropped_closed_total`,
  RESULT. RPC defteri: `rpc_sent = req_local + req_ext + Σ req_rej_* +
  req_refused + req_unread + requests_dropped_closed`.
- **Geride bırakılan parkın kanalı (turda bulundu):** `afk_action =
  leave_room` altında park edilen üyenin kanalını bırakan `release_actions`
  (B40) içindeki okunmamış istekleri saymıyordu (despawn sayıyordu). Artık
  `drop_unread_requests`'le bırakıp `requests_dropped_unread`'e katıyor
  (oda + shard).
- İki altın metin bilerek yalnız yeni aileler kadar değişti; `otlp::cross`
  yeşil. Loadgen metrik teli **GSMJ**. İstemci teli değişmedi.
- Turun taramasıyla bulunan diğer sayılmayan kayıplar: BACKLOG B53–B57.

Testler 1309 → 1318 (`otlp` ile 1327 → 1336): oda/shard kapalı-kanal
birim (2), `tests/room_close/forward_closed.rs` (2), oda/shard geride
bırakılan park (3), loadgen tel (2); önce kırmızı, mutasyonlar öldü.
Ebeveyn doğrulaması: kapalı kanal sayımını silmek iki testi düşürdü.

## Küçük paket 7 — F1, F4 (`misc/small-bundle-7`)

- **F1 — `MovementSystem` birim testleri.** Demo'nun hareket sistemi
  (`gsb-demo/src/demo/systems.rs`) artık tek başına test ediliyor
  (`systems/tests.rs`, 8 test): tick başına `speed·dt` doğru doğrultuda;
  `Speed` yoksa `DEFAULT_SPEED`; bir adım içindeyse hedefe TAM iniş +
  `MoveTarget` silinir (adım = kalan mesafe sınırı dahil); demo sistem
  yığını üzerinden spawn → hedef → varış (25 tick, aşma yok, sonra yazma
  yok); hedefinde duran varlık ve `dt ≤ 0` yazılmaz; varlıklar birbirinden
  bağımsız. 12 mutasyonun hepsi en az bir testi düşürdü. `MovementSystem`'in
  doc yorumu özel `Step` yapısına kaymıştı, yerine alındı. Arena'nın
  `Movement`'ının zaten iki birim testi var. Not (hata değil): hedefi
  bulunduğu konuma eşit varlık `MoveTarget`'ı hiç kaybetmez (yazma yok,
  zararsız).
- **F4 — Demo AOI/team/PVS odalarına `with_economy`.** `AoiRoomExt`,
  `TeamRoomExt`, `SectorRoomExt` `OpenRoomExt` ile aynı biçimde
  `with_economy` kazandı; `gsb-server`'ın aoi/team/pvs fabrikaları
  `game_mut().set_economy(…)` dolanması yerine builder zincirini kullanıyor.
  Davranış ve bayt aynı. Test `demo/rooms/tests/economy.rs` (5).
- Yan bulgu **B52** (BACKLOG): `loadgen_orchestrates_the_mmo` yükte bir kez
  `shard_members=0,0,0,0` ile düştü, tekrar koşuda yeşil.

Testler 1296 → 1309 (`otlp` ile 1314 → 1327). Ebeveyn doğrulaması: hareketin
`dy` işaretini çevirmek dört testi düşürdü.

## B36 — ayrılışta okunmamış istekler ve oda defterinin kapanışı (`rpc/b36-leave-accounting`)

- **Neden 1 (motor):** oda CONTROL'ü READ'den önce koşar; ayrılış,
  istemcinin son isteklerinden önce odaya varırsa `despawn_conn` satırı
  siler ve aksiyon kanalı içindeki isteklerle düşerdi — işlenmez,
  yanıtlanmaz, hiçbir kovada sayılmazdı. Yeni çekirdek sayaç
  `requests_dropped_unread`: kanalı götüren her yerde (oda + shard
  `despawn_conn`, aynı bağlantının yeniden katılımı, resume, shard'da
  ayrılıştan sonra düşen göç) kanal kapatılır, içindeki RPC istekleri
  sayılır (düz aksiyonlar değil). `gsb-metric` `req_unread=`,
  Prometheus/OTLP `gsb_room_requests_dropped_unread_total` (iki altın
  metin bilerek bu aile kadar), loadgen metrik teli **GSMH**, RESULT
  "her anahtar her zaman" kuralıyla `req_unread=`. İstemci teli değişmedi.
- **Neden 2 (loadgen):** oda sayaçlarını 1 Hz örnekler, dururken
  göndermez; süreç içi koşu son istemciden 150 ms sonra sunucuyu
  durdurduğundan RESULT'un `req_*`'ı bitişten bir periyoda kadar önceki
  örnekti (tam sayı süreler tesadüfen örnekle hizalandığından nadiren
  görünüyordu). Bekleme artık 150 ms + bir metrik periyodu
  (`final_sample_grace`); her süreç içi koşu ~1 sn uzar.
- **Defter kapanıyor:** `rpc_sent = req_local + req_ext + Σ req_rej_* +
  req_refused + req_unread` — her ölçüm satırında birebir (200 ve 500
  istemci, uzun duraklama ve kesirli süreler; RPC-CONTROL-PLANE §8.3).
- Elenenler: READ'i CONTROL'den önce koşmak / ayrılışı kanal boşalana
  dek ertelemek (ayrılmış oyuncu için yan etki), artık istekleri işleyip
  yanıtı atmak, mevcut bir kovayı yeniden kullanmak, odanın durunca son
  örnek göndermesi (toplayıcının kapanışıyla yarışır), düz aksiyonları da
  saymak.
- Kalıntı (BACKLOG B51): oda üyeliği kendisi bitirdiğinde bağlantı
  aktörünün kapalı kanala gönderdiği istek odaya ulaşmaz, sayılmaz.

Testler 1286 → 1296 (`otlp` ile 1304 → 1314; F27 ile birlikte): oda/shard
birim (5), `tests/rpc/unread.rs`, `tests/rpc_shard/unread.rs` (2),
loadgen tel, `loadgen_rpc.rs::the_rooms_ledger_covers_the_end_of_the_run`;
iki mevcut `loadgen_rpc` testi artık defter eşitliğini de iddia ediyor.
Her yeni test önce kırmızı; 12 mutasyon öldü. Ebeveyn doğrulaması:
yardımcının yalnız bir isteği sayması beş testi düşürdü.

## F27 — ayrılmanın nedeni politikaya (`kit/f27-disconnect-cause`)

- **Çekirdek: `DisconnectCause`.** Oda ve shard aktörü politikayı artık
  `GameLogic::on_disconnect_with(world, player, identity, cause)` ile
  sorar; neden aktörün ayırt ettiğidir: `ConnectionClosed` (registry'nin
  `ConnClosed` yolu — bağlantının neden kapandığı odaya gelmez),
  `IdleInput` (girdi-boşta tavanı, iki `afk_action`'da da), `Kicked`
  (`TickCtx::kick`). Sağlanan metodun varsayılanı `on_disconnect`'i
  çağırır: nedenden habersiz mantık bayt bayt aynı. Enum
  `#[non_exhaustive]`. B43'te yeniden katılan bağlantının yeni üyeliği
  `ConnectionClosed` ile biter (o oda yargılamadı). Yeni `ShardLogic`
  metodu yok (ortak üst-trait). Politikaya hiç ulaşmayan iki son: yeni
  oturumun eskisini devirmesi (`on_leave`) ve odanın kapanışı. RECONNECT §3.3.
- **Kit: nedene göre kopma politikası.** Her oda türünde
  `with_disconnect_policy_for(cause, grace, to)` oda geneli kuralı o
  neden için bütünüyle ezer — "atılan → despawn, düşen → park" çekirdek
  kodu yazmadan. Beş oda `on_disconnect_with`'i uygular, sharded
  spatial/team kompozitleri iletir. Ezme yoksa davranış aynı.
- Elenenler: `on_disconnect` imzasını değiştirmek ya da bağlam yapısı
  (her uygulayıcıyı kırar), ayrı neden bildirimi (iki çağrı arasında
  durum), neden başına kanca, `ServerClose`'u `ConnClosed` ile taşımak
  (tüketicisi yok — BACKLOG F28), kit'te `Game::disconnect_policy` kancası.
- Tel, `/metrics`, loadgen RESULT değişmedi.

Testler 1275 → 1286 (`otlp` ile 1293 → 1304): çekirdek oda/shard her
çağrı yeri, B43 yeniden katılma, kit yedi oda türü + gerçek aktörlerde
sharded ve tek dünya takım odası (atılan despawn, düşen park); mutasyonlar
öldü. Ebeveyn doğrulaması: ezme aramasında nedeni yok saymak üç testi
düşürdü.

## Küçük paket 6 — B37, B47 (`misc/small-bundle-6`)

- **B37 — pinsiz orkestratör çocuklara artık `--workers 1` vermiyor.**
  `--pin`'siz `--orchestrate` sunucu ve istemci çocuklarına
  `args.workers.max(1)` veriyordu: `--workers` yazılmadıysa her çocuk tek
  tokio worker'ında koştu (yorum "runtime default" diyordu; orkestratör
  yazıldığında doğruydu, düz/`--serve` varsayılanı `available_parallelism`'e
  çekilince geride kaldı). Şimdi (`child_args.rs::child_workers`): `--pin`
  altında çekirdek kümesinin boyu (değişmedi), pinsiz + açık `--workers N`
  → iki çocuğa N, aksi hâlde `--workers` hiç iletilmez — çocuk kendi
  varsayılanında. Eski pinsiz orkestre tabanları tek worker'lıdır ve bu
  turda yeniden ölçülmedi (liste RPC-CONTROL-PLANE §8.2 "Pinsiz
  orkestratörün worker sayısı"; BACKLOG B50). B32'nin tekrarlanan
  `dropped` 116'sı da bu koşulun ürünüydü.
- **B47 — ops HTTP istek başlığına süre sınırı.** Bağlanıp tek bayt
  göndermeyen (ya da bayt bayt damlatan) eş bağlantı görevini kopana dek
  tutuyordu. Başlığın TAMAMI `HEAD_DEADLINE` = 5 sn içinde gelmeli
  (`http/head.rs`, okumanın etrafında tek `timeout` — okuma başına değil);
  aşılırsa tek `408 Request Timeout` + normal kapanış (300 ms boşaltma).
  Eşzamanlı ops bağlantı tavanı ve yanıt yazmanın süre sınırı hâlâ yok
  (BACKLOG B49; localhost sözleşmesi). gsb-server dev-dependency'lerine
  tokio `test-util` (paused saat).

Testler 1269 → 1275 (`otlp` ile 1287 → 1293); her yeni test önce kırmızı
görüldü, dokuz mutasyon öldü. Ebeveyn doğrulaması: açık `--workers N`'i
yalnız sunucu çocuğuna iletmek iki testi düşürdü.

## B43 — kapatma hükmü yeniden katılan bağlantıya da düşer (`core/b43-close-race`)

- Oda üyeliği bitirip (`afk_action = disconnect` tavanı ya da E8 atması)
  bağlantının kapatılmasını istediğinde istek dolu registry posta
  kutusunun arkasında beklerken istemci kapalı aksiyon kanalını görüp
  (`ERROR 6`) yeni varlıkla yeniden katılabiliyordu: geç gelen istek
  `room`+`entity` korumasında bayat sayılıyor, soket açık kalıyor, atılan
  istemci atmadan kurtuluyordu (`LEAVE` + `JOIN` ile de).
- Düzeltme yalnız registry'de (`registry/actor/close.rs`): koruma artık
  yalnız TABLO yerleşimini korur; isteğin üyeliği satırın şimdiki
  üyeliği değilse tablo olduğu gibi kalır, hüküm (`ConnIn::ServerClosed`)
  yine bağlantıya gider. `ConnectionId` süreç ömrü boyunca tekil
  olduğundan istek yalnız kendi bağlantısını adlandırabilir. Yeni üyelik
  bağlantının kapanışıyla taşıma-ölümü yolundan biter (`on_disconnect`
  bir kez). `LeaveConn` (B40) değişmedi. Oda/shard kodu, tel, `/metrics`,
  loadgen RESULT aynı.
- Değişen mevcut test: `registry.rs`'deki "bayat istek no-op" testi tam
  da hatayı sabitliyordu (bağlantıya hiçbir şey söylenmez); artık yeni
  kuralı sabitler — tablo yerleşmez, bağlantı söylenir (ayrılmadan sonra
  da); bilinmeyen bağlantı hâlâ no-op.
- Kabul edilen: aynı odaya doğrudan yeniden katılmada eski üyeliğin sonu
  kümülatif `leaves`'te sayılmaz (registry bunu LEAVE+JOIN'den ayıramaz;
  mevcut anlambilim, sızıntı değil).
- Elenenler: odanın bağlantıya doğrudan söylemesi (yeni tutamaç her yere,
  B12 gereği yine düşebilir), isteğin bekletilen/spawn'lı gönderimi,
  yeniden katılmayı reddetmek, biten üyelik defteri.

Testler 1267 → 1269 (`otlp` ile 1285 → 1287): `tests/room_close/
rejoin_races.rs` (+ kapatma isteklerini tutan röle `rejoin_rig.rs`) —
tavan ve atma × tek oda ve ızgara; önce düştü, sonra ERROR 9 + kapanış,
her üyelik bir kez biter, sızıntı yok; üç mutasyon öldü. Ebeveyn
doğrulaması: eşleşmeyen dalda hükmü yalnız atmaya iletmek tavan testini
düşürdü. RECONNECT §16.4.

## E8 — oyun mantığına "oyuncuyu at" fiili (`core/e8-kick-verb`)

Bakımcı kararı (2026-09-27, E9 ile birlikte): atma = bağlantıyı kapatmak;
istemcinin "odadan çıkarıldın, bağlantın açık" diye bilmesine gerek yok.
Üyelik `on_disconnect` ile biter (kaderi oyunun `Detach`'ı seçer: park /
AI devri / despawn), ardından soket E6'nın fiiliyle kapanır. Yeni tel
öğesi yok, base protokol sürümü aynı.

- **Yüzey:** `TickCtx::kick(player, reason)` (bağlamda `kicks: Kicks<'a>`
  alanı; `KickQueue`, `Kick`, `KICK_REASON_MAX_BYTES = 256`,
  `kick_message`). `GameLogic`/`ShardLogic`'e yeni metot yok. Kit:
  `gsb_kit::game::kick(world, entity, reason)` — yedi kit odası sahibi
  çözüp oyunun sistemlerinden hemen sonra çekirdeğe iletir; sahipsiz
  entity yok sayılır.
- **Uygulama noktası:** istenen kancanın içinde asla. Girdi/istek/sistem
  kancalarında sorulan SYSTEMS'tan sonra (shard'da EFFECTS OUT'tan sonra,
  MIGRATE'ten önce — atılan üye aynı tick göçmez), yayın kancalarında
  sorulan tick sonunda; arada CONTROL koşmaz. Kapatma isteği E6 kuyruğuyla
  sonraki tick 0d'de.
- **Tel:** en-iyi-çaba, beklemesiz ERROR 9, `kicked: <gerekçe>` (gerekçe
  256 bayta `char` sınırında kesilir; boşsa `kicked`), sonra kapanış.
- **Sayaç:** `server_closes{reason="kicked"}` (sona eklendi); iki altın
  dosya tam bu satırla (OTLP'de seri sayısı 12 → 13) güncellendi; log
  satırında `server_close_kicked=`; loadgen metrik teli **GSMG**; RESULT
  "her sebep için bir anahtar" kuralıyla `server_close_kicked=` kazandı
  (E6'nın `server_close_idle_input=`'u gibi; biçim aynı).
- **Kenar durumları:** canlı olmayan üye (bilinmeyen/gitmiş/park/bot) →
  sayılmayan no-op; aynı tick çift atma → tek kapanış, ilk gerekçe; dolu
  posta kutusu → E6 kuralı; shard'da MIGRATE sonrası sorulan ve göçmüş
  üye → no-op. B43'ün yarışı atmada da geçerli (düzeltilmedi).
- **Kit varsayılanı:** kit odaları kimliği olan oturumu park eder — atılan
  oyuncu aynı kimlikle resume edebilir; nedene göre kader BACKLOG F27.
- **Yan bulgu B48 (düzeltildi):** shard tick gövdesi input-idle saatini
  MIGRATE'ten sonra geri alıyordu — göçen üyenin `last_input`'u hep
  `None` gidiyor, alıcı saati başlatmıyordu (sınır geçişi AFK tavanını
  kalıcı atlatıyordu). Saat artık MIGRATE'ten önce geri alınıyor.
- `TickCtx` artık `Send`/`Sync` değil (kuyruk bir `Cell`); tick gövdesi
  await etmediğinden hiçbir görevi etkilemez.

Testler 1237 → 1267 (`otlp` ile 1255 → 1285): çekirdek oda/shard, kit
(yedi oda + gerçek shard aktörleri ve tek takım odası aktörü), gerçek
dinleyici üzerinde uçtan uca; her kural mutasyonla sınandı. Ebeveyn
doğrulaması: belge bağlantısını kıran ara commit bir sonrakiyle katlandı
(her commit doc kapısından geçer); `on_disconnect` çağrısını silmek ve
saati MIGRATE'ten sonra geri vermek — ikisi de testleri düşürdü.
Ayrıntı: RECONNECT §16.3, GAME-MODULE, KIT-ARCHITECTURE §4.3, OPS §3.

## Küçük paket 5 — B34, B39, B46, B33 (`misc/small-bundle-5`)

- **B34 — CI action'ları Node 24 ana sürümlerinde.** `actions/checkout@v4`
  → `@v5` (7 iş), `actions/upload-artifact@v4` → `@v6` (autobahn) — node20
  çalışma zamanı kaldırılıyor, ilk CI koşusu uyarıp zorla Node 24'te
  koşturmuştu. `dtolnay/rust-toolchain` composite (Node yok);
  `Swatinem/rust-cache@v2` zaten node24. Ebeveyn `action.yml`'leri
  kaynağından doğruladı (`checkout@v5`, `upload-artifact@v6`,
  `rust-cache@v2`: `using: node24`; `upload-artifact@v5` hâlâ node20).
  Kural CONTRIBUTING'de.
- **B39 — kare sayaçları dışa açımda.** `RoomReport::shipped_frames` /
  `private_frames` aile tablosuna iki kümülatif sayaç olarak girdi:
  `gsb_room_shipped_frames_total`, `gsb_room_private_frames_total`
  (Prometheus `counter`, OTLP monoton kümülatif `Sum`); iki altın metin
  bilerek bu iki aile kadar değişti, `otlp::cross` yeşil. `gsb-metric`
  satırında zaten vardı. OPS §3, DESIGN §12.
- **B46 — kararlı pencerenin sonu tutarlı kesitten.** `last_steady`
  (`records_per_tick`'in ve savaşın takım penceresinin sonu) eskiden
  `members == peak_members` olan herhangi bir raporun en yenisiydi;
  toplamı tesadüfen tepeye eşit yırtık/eksik bir rapor pencereyi
  bitirebiliyordu. `spread::steady_end` artık `steady_span`'in sonunu
  (nüfus raporları, kesitsiz koşuda yırtık geri düşüş) verir. RESULT
  biçimi aynı.
- **B33 — ops HTTP accept döngüsü kapıyla durur.** `http.abort()` yerine
  ops yüzeyinin accept'i oyun kapılarının `Door`'undan geçer; `stop()`
  kapıyı kapatır, döngü döner ve listener'ı düşürür, dinleyicilerin
  döngüleriyle aynı 1 sn'lik son tarih altında beklenir (abort yalnız
  geri sigorta). `StopReport`'a alan eklenmedi: ops döngüsü
  `accept_loops_ended`'a +1 sayılır. DESIGN §9, OPS.
- Yan bulgu **B47** (BACKLOG): ops HTTP `read_head`'in süre sınırı yok.

Testler 1234 → 1237 (`otlp` ile 1252 → 1255); her yeni test önce
kırmızı görüldü, düzeltme mutasyonları (B39 okuyucu, B46 kesit/geri
düşüş, B33 kapalı-hata kolu/kapıyı kapatmamak) testleri düşürüyor.
Ebeveyn doğrulaması: `steady_end`'in pencerenin ilk raporunu vermesi ve
ops görevinin beklenen döngülere eklenmemesi — ikisi de kırıldı.

## F6 — seam ötesi alan sorgusu: `local ∪ lent` kit'te (`kit/f6-area-query`)

- **Kit'in yapı taşı (opt-in, varsayılan değişmedi):** `Seam::find`
  (tek wire), `Seam::area` (oyunun yüklemi) ve `Seam::within` (disk,
  sınır dahil) — her wire BİR kez, wire sırasıyla, oyunun tek görünüm
  tipiyle (`SeamView<V>`: kendi entity'den ya da ödünç kayıttan) ve
  nerede yaşadığıyla (`Holder::Local(entity)` / `Holder::Lent { lender }`).
  Öncelik seam'inki: yerel (dünya sorgusunun bulduğu) > devreden (yeni
  sahip, ayrıldığı kayıt — D) > ödünç (iki kiralayanlı wire'da düşük
  indeks — `emit`'in rotası); takım ithalatı (W1) bulunmaz. Maliyet:
  sahip tablosu + ödünç kayıtlar üzerinde doğrusal geçiş, isabetlerin
  yerinde sıralanması; çağrı başına tahsis yok.
- **`lent_iter` düzeltmesi:** eski sahip despawn edeceği kopyayı hâlâ
  export ederken yeni sahip de ödünç veriyorsa (ikisine komşu üçüncü
  shard — 8-komşuluk) wire'ı iki kez veriyordu (gerçek aktörlerde
  gözlendi); artık yalnız düşük kiralayanınkini verir.
- **Savaş benimsedi:** saldırı ve uzak darbenin kaynak denetimi
  `Seam::find` + tek görünüm (`Foe`) ile tek denetim; hedefler ve retler
  aynı (önce kilitlenen test), tel dokunulmadı. MMO olduğu gibi (BACKLOG F26).
- Envanter: hiçbir demo seam ötesi ALAN sorgusu yapmıyordu (savaşın
  ele geçirmesi haritayla kaçınıyor); ikisi de nokta biçimini ("yerel,
  değilse ödünç") elle yazıyordu.

Testler 1225 → 1234 (`otlp` ile 1252); gerçek dört shard aktörü testi
dahil; kit'te 13 mutasyondan 11'i kırıldı, 1'i eşdeğer, 1'i testin
güçlendirilmesiyle kırıldı; savaşta eski kodda 8/8, yeni kodda 9/10
(sağ kalan gözlemlenemez: `strike` ayakta olmayanı zaten reddeder).
Ebeveyn doğrulaması: `within`'in `dy` terimini düşüren ve ödünç
`Foe`'nun can denetimini silen iki bağımsız mutasyon da kırıldı.
Ayrıntı: KIT-ARCHITECTURE §4.6 "F6 eklemeleri", §10 "F6"; CROSS-SHARD §4b.

## Küçük paket 4 — B44, B45, F24 (`misc/small-bundle-4`)

- **B44 — başlangıç odaları her join'in önünde.** `room_count` odalarının
  `CreateRoom`'ları eskiden spawn'lı görevlerden gidiyor, accept döngüleri
  beklemeden açılıyordu; hemen katılan istemci `RoomOpFailed "room 1 not
  found"` alabiliyordu. Artık `boot/start/boot_rooms.rs` hepsini accept
  döngüleri spawn edilmeden önce, id sırasıyla, registry mailbox'ına satır
  içinde (`try_send`) koyar; registry FIFO boşalttığı için her join
  onların arkasındadır. Yalnız cevaplar spawn'lı görevde beklenir
  (başlatma bir odayı beklemez; S'nin kuralı korunur); boş kapasiteyi
  (4096) aşanlar spawn'dan sıralı + `warn`. Kilit `tests/boot_rooms.rs`
  (düzeltmeden önce 5/5 `Absent`).
- **B45 — tutarlı kesit her shard'ın satırını içerir.** Eksik satırlı
  rapor kesit sayılıyordu; kesit artık `rooms.len() == shards` ister
  (shard sayısı RESULT'un `shards=` eşlemesinden). RESULT biçimi aynı.
- **F24 — AFK varsayılan testinin yoklaması** kendi numaralı
  heartbeat'lerinden biri cevaplanana dek yineler (1/sn ACK kısmasına
  karşı; ERROR/kapanış yine başarısızlık). Hog altında 191/192 → 384/384.

Testler 1220 → 1225 (`otlp` ile 1243); ajanın mutasyonları yakalandı;
ebeveynin bağımsız mutasyonu (bir satırı eksik raporu da kesit saymak) 2
testi kırıyor.

## F23 — yükte düşen gerçek saatli testler (`test/f23-flaky`)

Yük altında (1 dk load 15+) ara sıra düşen testler sistematik arandı: 30
ve 64 meşgul döngülük CPU yükü altında (32 çekirdek) tam süit, varsayılan
ve `otlp`: önce 30 koşu (7 düşüş, 3 test), düzeltmeden sonra 64 yükte 20
koşu 0 düşüş; ayrıca şüpheliler tek başına 64–300 kez paralel ve statik
tarama. Yalnız testler değişti, motor değişmedi.

- `client::accounting::tests::ws_bytes_are_the_messages_on_the_wire`:
  sahte WS peer tamponsuz okuduğu için flood birikimini 500 ms'lik LEAVE
  penceresinde eritemiyordu (~%11). Peer tamponlu okur.
- `metrics::tests::collector::room_counters_flow_to_collector` ve
  `metrics::tests::export::…_in_order`: toplayıcının periyodu duvar
  saatinde; "250/80 ms'de ≥ N" pencereleri zamanlayıcıya bağlıydı. Artık
  durumu bekler; toplayıcı testi her ardışık rapor çiftinde birikimli
  sayaçların azalmadığını da denetler (öncekinden güçlü).
- `loadgen_drives_the_mmo`: 4 sn'lik koşuda yükte yeterli tutarlı kesit
  çıkmıyordu (sözleşme uçuştaki oyuncuyu bir kesitten düşürebilir); koşu 8
  sn, iddia tam eşitlik (±1 mutasyonu düşer).
- Kural CONTRIBUTING "Gerçek saatli testler"de: tick/motor süresi sayan
  test paused saatte; duvar saatine bağlı test koşulu bekler, sabit
  pencerede saymaz; sahte uç ölçülen kodun darboğazı olmaz.
- **Yan bulgular (düzeltilmedi → BACKLOG):** B44 boot odası yarışı, B45
  loadgen'in eksik satırlı "tutarlı kesit"i, F24 `afk_action` heartbeat
  yoklaması.

Test sayısı değişmedi (1220; `otlp` 1238). Ebeveynin bağımsız mutasyonu
(WS istemci maskesini saymamak) düzeltilen testi hâlâ kırıyor.

## B41 — `Disconnect` + park: bekleyen kapatma isteği parkın raporunun arkasında (`fix/b41-close-ordering`)

`afk_action = disconnect` altında dolu registry posta kutusunun arkasında
`parked` diye bekleyen kapatma isteği, park beklerken despawn ile biterse
(kısa/sıfır grace) yanlışa düşüyordu: parkın `DetachDespawned`'ı 0c'de,
isteğin 0d gönderiminden ÖNCE gidiyor; registry satır henüz `detached`
olmadığı için raporu bayat yankı sayıp düşürüyor, ardından gelen `parked`
istek satırı `detached` işaretliyordu — satır slotunu aynı kimlik dönene ya
da oda bitene dek tutuyordu (SIZINTI). Tek slotluk dolu posta kutusu + adım
adım boşaltılan vekil registry ile deterministik kuruldu.

- Oda ve shard, bekleyen isteğin `parked`'ını her gönderim denemesinden
  önce yeniden sınar (`reconcile_closes`): oda bağlantının bir üyeliğini
  hâlâ tutuyorsa (park, bot, yeniden alınan üyelik) `parked` kalır; yoksa
  istek despawn olarak yerleşir — iki varış sırası da aynı sona varır.
- Elenen: B40'ın "rapor önünde bekleme" kuralı (rapor kuyrukta beklemez,
  0c'de gider); tek sıralı giden kutusu; registry'de raporun canlı satırı
  bırakması (kapatılmayı bekleyen bağlantının satırını silerdi).
- Kabul edilen sınır: shard'da beklerken göç eden park — eksik sayım,
  sızıntı değil. Kalan kenar B43 (RECONNECT §16.2).

Testler 1216 → 1220 (`otlp` ile 1238); 2 test düzeltmeden önce kırmızı;
ajanın 4 mutasyonu yakalandı; ebeveynin bağımsız mutasyonu (koşulu ters
çevirmek) 3 testi kırıyor.

## B40 — varsayılan idle-kick'in iki kusuru (`fix/b40-idle-kick`)

`afk_action = leave_room` altında tavan üyeliği bitiriyordu ama: park
edilen üyenin satırı bağlantının aksiyon kanalını tutuyordu (kareler
parkta birikiyor, doğrudan JOIN ERROR 3 — sert ihlal); despawn edilen
üyenin registry satırı canlı aidiyet kalıp soket kapanınca slotuyla
sızıyordu. Sözleşme (RECONNECT §16): idle-kick'ten sonra bağlantı kendi
`LEAVE_ROOM_REQ`'inden sonraki durumdadır — odada değil, oyun kareleri
`ERROR 6` (race, sert ihlal değil), JOIN doğrudan (park varsa örtük
resume, dolu ızgarada da); kick anında tel bayt bayt aynı (protokolde
"odadan çıkarıldın, bağlantı açık" karesi yok — B42).

- `ConnectionId::park_key()` (üst bit ayrılmış): canlı bağlantının geride
  bıraktığı park bu anahtara taşınır ve iki kanal yarısı bırakılır —
  taşıma ölümünün şekli.
- Oda/shard → registry `RegistryMsg::LeaveConn(LeaveRequest)` (E6 kuyruk
  kuralları + rapor önünde bekleme); registry despawn'ı E6'nın
  `settle_ended`'ı ile, parkı kendi `detached` satırına taşıyarak
  yerleştirir; bağlantıya `ConnIn::LeftRoom`.
- Izgara cap'i kimliğin kendi parkını resume eden join'i reddetmez (taşıma
  ölümünden sonra dolu ızgaraya dönüşü de düzeltir); başka odaya join,
  bildirilmemiş bir bitişin slotunu geri verir.

Testler 1192 → 1216 (`otlp` ile 1234); 7 test düzeltmeden önce kırmızı;
~25 mutasyon yakalandı. Ebeveynin bağımsız mutasyonu (park anahtarının
canlı kimlikle çakışması) 7'den fazla testi kırıyor.

## E6 — girdi-boşta tavanının eylemi: opt-in kapatma fiili (`core/e6-close-verb`)

Tavan (`max_idle_input_secs`) oda ÜYELİĞİNİ bitiriyor, soketi açık
bırakıyordu; odanın bir bağlantıyı kapattırma yolu yoktu. Kullanıcı kararı
(2026-09-27): opt-in kapatma fiili, varsayılan bugünkü.

- `RoomConfig::afk_action: AfkAction` — `LeaveRoom` (varsayılan, bayt bayt
  bugünkü) / `Disconnect`. İkisinde de önce oyunun `on_disconnect`'i
  varlığın kaderini seçer; `Disconnect` bağlantıyı da kapatır.
- Oda→registry fiili: `RegistryMsg::CloseConn(CloseRequest { conn, room,
  entity, parked, cause, reason })`; oda/shard 0d fazında `try_send` eder;
  DOLU posta kutusunda istek kuyrukta kalır ve sonraki tick yeniden
  denenir, KAPALIDA düşer. Registry satırını tek aramada yerleştirir ve
  kararı spawn'lı `ConnIn::ServerClosed` ile iletir.
- İstemciye en-iyi-çaba, beklemesiz ERROR 9 (`input idle: …`), sonra
  kapanış. Yeni `server_closes{reason="idle_input"}`; iki altın dosya
  bilerek güncellendi; loadgen teli **GSMF**.
- Config `afk_action = "leave_room" | "disconnect"`, düz ve `[rooms.<id>]`;
  katman: çekirdek → `GameModule::afk_action()` → düz → `[rooms.<id>]`
  (oyun varsayılanları `GameDefaults { input_rate, afk_action }`).
- Oyun mantığına genel "oyuncuyu at" fiili AÇILMADI (BACKLOG E8).
- **Yan bulgu B40** (varsayılan yolda, önceden vardı): park edilmiş idle
  üyenin istemcisi LEAVE'siz yeniden katılamıyor (JOIN ERROR 3, sert
  ihlal); despawn edilen üyenin registry satırı soket sonra kapanırsa
  slotuyla sızıyor. `disconnect` ikisini de yaşamaz.

Testler 1161 → 1192 (`otlp` ile 1210); ajanın ~20 mutasyonu yakalandı;
ebeveynin bağımsız mutasyonu (idle kapanışında bildirimi göndermemek) 4
testi kırıyor. Ebeveynin bir `otlp` koşusunda bir test yük altında (load
~15) bir kez düştü, sonraki dört koşu yeşil — kimliği yakalanamadı (F23).

## E1 + F21 — girdi hız sınırı (opt-in) ve sıfır aksiyon kapasitesi (`core/e1-input-rate`)

Auth-sonrası GEÇERLİ girdinin saniye başına hacmi sınırsızdı; tek sınır
odanın per-tick çekme bütçesiydi (odayı korur, göndericiyi değil).
Kullanıcı kararı (2026-09-27): opt-in yapı taşı.

- `RoomConfig::input_rate: Option<InputRate>` (`InputRate { per_sec,
  burst }`; `None` = KAPALI, varsayılan): bağlantı başına token bucket.
  Bağlantı aktöründe, protokol kontrollerinden sonra ve odanın kanalına
  `try_send`'den önce uygulanır (`conn/gate.rs`): aşan girdi odaya hiç
  girmez, DÜŞER, `input_rate_limited` sayılır, ihlal DEĞİL. Yalnız kayıtlı
  oyun-bandı opcode'ları; kontrol bandı ve `RPC_REQ` ölçülmez.
- Sayı odanın: registry join'de odanın config'inden damgalar (yeni
  `registry::Seat { entity, actions, input_rate }`). Kova bağlantınındır:
  join yeniden ayarlar, doldurmaz (leave/join döngüsü burst satın alamaz).
- Saat `ticker::now()`; O(1), tahsissiz, zamanlayıcısız (nano-jeton, u128).
- Sayaç yolu: `ConnSample` → `NetReport` → satır `input_rate_limited=` →
  aile tablosu (Prometheus `gsb_net_input_rate_limited_total`, OTLP
  `gsb_net_input_rate_limited`) → loadgen teli **GSME**.
- Config: `input_rate_hz` / `input_burst`, düz ve `[rooms.<id>]`; katman:
  çekirdek (kapalı) → oyunun sayısı (`GameModule::input_rate()`) → düz →
  `[rooms.<id>]`; `input_rate_hz = 0` kapalı; burst yazılmazsa bir
  saniyelik.
- **F21:** `conn_action = 0` artık oda aktörünü panikletmiyor — dört kanal
  kurulumu `RoomConfig::action_channel` üstünden `max(1)`'e kıstırılıyor;
  sunucu `0`'ı başlatmada reddediyor (yeri adlandırarak).
- Varsayılan davranış ve istemci tel baytı değişmedi.

Testler 1131 → 1161 (`otlp` ile 1179); ajanın 46 mutasyonu yakalandı;
rebase E2'nin aile tablosuna taşıdı (ajan). Ebeveynin bağımsız mutasyonu
(kova boşken de kabul) 6 testi kırıyor.

## E2 — dışa açım katmanı: takılabilir exporter'lar (`ops/e2-exporters`)

Kullanıcı kararı (BACKLOG E2): içeride ucuz toplama aynı kalır, dışa açım
takılabilir exporter'lara devredilir, her biri feature arkasında.

- Dikiş: `gsb_core::metrics::Exporter` (`fn export(&mut self,
  &MetricReport)`); toplayıcının `emit`'i TEK dışa açım yeri — exporter'lar
  sırayla, sonra sink. Exporter saf tüketici, bloklamaz; aktör kodu
  değişmedi.
- Prometheus çekme olarak kaldı (kazıma anında render), `prometheus`
  feature'ına (varsayılan açık) alındı; feature'sız derlemede `/metrics`
  404 + feature adı. `gsb-core` workspace'e varsayılan feature'sız
  bağlanıyor, exporter'lara `gsb-server` karar veriyor.
- Tek aile tablosu (`metrics::export::families`): Prometheus ve OTLP aynı
  adı/türü/help'i/sırayı yürüyor; Prometheus metni bayt bayt aynı (altın
  test değişmedi).
- OTLP exporter'ı (`otlp` feature'ı, varsayılan kapalı): OTLP/HTTP
  protobuf POST; counter → monotonic kümülatif Sum, gauge → Gauge, adım
  histogramları → kümülatif Histogram, oda/sebep öznitelik,
  `service.name` resource'ta. Tek yuvalı devir: dolu yuva düşürür ve
  sayar; itme hatası sayılır, yeniden denenmez. **Yeni bağımlılık yok**
  (elle `prost` derive'lı mesaj alt kümesi + tokio `TcpStream`; alan
  numaraları resmî `opentelemetry-proto` + `protoc --decode` ile bir kez
  doğrulandı).
- Config `[metrics.otlp]` (`endpoint`, `interval_secs` = 10,
  `service_name` = `"gsb"`); feature'sız derlemede tablo başlatmayı
  `ServerError::OtlpNotBuilt` ile durdurur. CI'a `otlp` işi.

Testler 1124 → 1131 (`otlp` ile 1149); ajanın 26 mutasyonu yakalandı;
ebeveynin bağımsız mutasyonu (aile tablosunda bir getter'ı kaydırmak) hem
Prometheus hem OTLP golden testini kırıyor.

## B23 — loadgen RPC trafik modu; RPC yolunun ilk yük ölçümü (`loadgen/b23-rpc-mode`)

Loadgen hiç istek göndermiyordu: odanın istek alımı, bağlantı başına cap,
ret kovaları, worker tamamlanmaları, F14'ün düşen batch'ler üzerinden
teslimi ve fırtına sınırı yük altında hiç koşmamıştı.

- Çekirdek: ret nedeni metinleri `gsb_core::rpc` sabitleri
  (`CONN_CAP_REASON`, `ROOM_CAP_REASON`, `DUPLICATE_REASON`,
  `MALFORMED_REASON`, `no_handler_reason`); baytlar aynı (birim testi
  sabitler).
- Loadgen `--rpc-rate R [--rpc-burst B]` (yalnız demo, yalnız düz istemci
  koşusu): demo'nun `ECONOMY` isteği B'lik patlamalarla her B/R sn'de.
  İstemci başına defter: ilk yanıt kapatır ve nedenine göre sayılır;
  ikinci yanıt `dup`, gönderilmemiş id `unmatched` (ikisi de 0 olmalı);
  istemci zaman aşımı = sunucu zaman aşımı + 1 sn. RESULT'ta `rpc_*`
  anahtarları yalnız modda; varsayılan satır birebir aynı.
- **Ölçüm** (RPC-CONTROL-PLANE §8.2): demo 200/500 × 1 ve 10 istek/sn, B=8
  patlama, F11 duraklamalı koşular. Her koşuda dup = unmatched = 0 (137 746
  düşen batch üzerinden bile — F14 yük altında tutuyor); istemci ve oda
  cap retlerinde birebir aynı (73 292); `sent = req_ext + req_refused`;
  ok p50 ≈ 50 ms, p99 ≈ 68 ms; istek başına tick maliyeti ~5 µs (5 000
  istek/sn'de +~745 µs, bütçe aşımı %0); oda cap'i bağlamadı; 3 sn
  duraklamada fırtına sınırı 36 837 isteği yanıtsız reddetti ve istemcinin
  yanıtsız/açık sayısı tam bunu verdi.
- **B32 açıklandı:** `dropped` = 116 tekrarlanıyor ama KATILMADA değil
  AYRILIŞTA: istemci LEAVE sonucunu alınca soketini kapatıyor, oda
  ayrılışı sonraki tick'te öğrenene dek fan-out kapalı kanala bir batch
  deniyor (`try_send` → Closed, bağlantı başına 1); kare kaybı yok. Pinsiz
  orkestratör çocuklara `--workers 1` veriyor (yorumu "runtime default"
  diyor) — sayı zamanlamaya bağlı (varsayılan worker'larla 0). Düzeltilmedi
  (metrik anlamı ve orkestre tabanları kararı).

Testler 1107 → 1124 (rebase sonrası, +17); ajanın 13 mutasyonu yakalandı;
ebeveynin bağımsız mutasyonu (geç yanıtı `late` saymamak) defter testini
kırıyor.

## B18 — oda başına config override (`server/b18-room-overrides`)

Sunucunun her odası aynı `RoomConfig`'i alıyordu; operatör yoğun bir odaya
ya da bir lobiye farklı oda ayarı veremiyordu.

- `[rooms.<id>]` (nokta yazımı `rooms.7.max_players = 64` de aynı): oda
  düzeyindeki sekiz anahtar (`tick_hz`, `room_control`, `conn_action`,
  `max_snapshot_bytes`, `keepalive_hz`, `max_players`,
  `max_idle_input_secs`, `max_detach_hold_secs`), düz anahtarlarla aynı
  yazım ve anlam; yazılmayan anahtar sunucunun değerinde kalır. Oyunun
  tabloları ayrı kalır (oda anahtarları motorun).
- Tek katmanlama noktası `RoomTemplate::room(id)` — başlangıç odaları,
  admin `/rooms/open` ve `Config::room_config` aynı odayı kuruyor.
- Öncelik: çekirdek varsayılanı → düz anahtarlar → `[rooms.<id>]` → admin
  `tick_hz` query'si (çatışmayı idempotent karşılaştırma 409'la yakalar).
- Doğrulama: bilinmeyen / oda düzeyi olmayan anahtar ve düz yazılmamış
  pozitif id ayrıştırmada hata; registry'nin reddedeceği oda başlatmada
  `ServerError::RoomOverride` — kural çekirdekte tek (`RoomConfig::
  step_divisor`, registry'nin create'i de onu çağırıyor). `room_count`'un
  ötesindeki id geçerli (runtime odası), `info` satırı.
- Varsayılan değişmedi (F8'in alan alan testiyle kilitli); tel baytı aynı.
- Bulgu F21 (önceden vardı): `conn_action = 0` join'de `mpsc::channel(0)`
  panik yolu.

Testler 1091 → 1107 (+16); ajanın 14 mutasyonu yakalandı; ebeveynin
bağımsız mutasyonu (override'daki `max_snapshot_bytes`'ı uygulamamak)
katmanlama testini kırıyor.

## F5 — servislerin açık durdurması (`server/f5-service-stop`)

Bir oyun servisi (demo'nun ekonomi servisi) yalnız son göndericisi
düşünce bitiyordu: `stop()` karşısında sonu örtük ve sırasızdı — odanın
son `on_shutdown`/`match_result`'ı servise yazdığında servisin ayakta
olacağının ya da `stop()`'tan sonra yarıda kesilmeyeceğinin güvencesi
yoktu (DESIGN §9.2).

- Çekirdek yapı taşı `gsb_core::service`: düşme bariyeri (`hold()` →
  `Hold`/`Released`) ve `Service` (görev + senkron durdurma isteği).
  `Registry::with_rooms_hold` token'ı her oda/shard ölüm bekçisine verir;
  bekçi oda görevi bitince bırakır — registry hiçbir odayı beklemez.
  Registry'nin `post_stop` deyimi `gsb_core::channel::post` olarak public.
- `RegistryParts::service(Service)` (isteğe bağlı kayıt). `stop()`:
  accept'lerden sonra odaların bariyeri (≤ 1 sn) → her servise istek
  (bant içi — kuyruktakiler önce işlenir) → tek son tarih altında join
  (≤ 1 sn), aşan abort. `StopReport` yeni alanlar: `rooms_finished`,
  `services_ended`, `services_aborted`. Kaydedilmeyen servis eski
  hayatını sürer.
- Ekonomi benimsedi: `EconomyService::start` → `(tutamaç, Service)`;
  `Stop` sonrası yeni istek almaz, borçlu cevapları teslim edip biter.
- Tel baytı değişmedi, yeni bağımlılık yok; `stop()` en kötü +2 sn
  (in-tree ~1 tick).

Testler 1077 → 1091 (rebase sonrası, +14); ajanın 14 mutasyonu yakalandı.
Ebeveynin bağımsız mutasyonu (bariyerin beklemeden açılması) 4 testi
kırıyor.

## F18 — `shard_members` N+1 kırılganlığı: loadgen yırtık raporu topluyordu (`fix/f18-shard-members`)

Metrik raporu her üreticinin SON örneğidir; shard'ların örnekleri
toplayıcıya bağımsız ulaşır ve CPU yükü altında bir rapor bazı shard'ların
`k` turu satırlarını diğerlerinin `k − 1` satırlarıyla yan yana
taşıyabilir. İki tur arasında göçen oyuncu iki satırda birden görünür
(kanıt, 30 süreçlik yükte başarısız koşu: `0:s60 m3 in1 | 1:s30 m2 out0 |
2:s60 | 3:s30` → 9; Σin > Σout, gerçek bir anda imkânsız). Loadgen tepe
nüfusu raporların EN BÜYÜK toplamı olarak aldığından tek bir yırtık 9 hem
`peak_members` hem kararlı pencere oluyordu.

- **Çekirdek doğru:** göçen oyuncu kaynaktan kesinleşme tick'i `h`'de
  çıkar, hedefe kurulumda (en erken `h + 1`) girer; aynı ya da bir tick
  arayla alınmış iki örnek onu iki kez sayamaz; uçuştaki oyuncu tek tick
  hiçbir yerde sayılmaz — sözleşme yazıldı (DESIGN §12 "tutarlı kesit",
  CROSS-SHARD §4d) ve çekirdek testiyle kilitlendi. Prometheus'ta
  shard'lar üzerinde `sum` aynı yırtılmaya açık (belgelendi).
- **Düzeltme loadgen'de:** bir raporun satırları yalnız tutarlı kesitse
  (her satır aynı `(steps, lagged_ticks)`) nüfus olarak toplanır; hiç
  kesit olmayan koşu eski yoldan okunur ve insan-okunur blok `(torn: …)`
  der; kalıcı bir çift sayım hâlâ görünür. RESULT anahtarları aynı.
- 30 süreçlik yük altında `loadgen_drives_the_mmo` 20/20 yeşil (önce 3/10
  kırmızı).

Testler 1071 → 1077; ebeveynin bağımsız mutasyonu (kesit kontrolünde
`steps`'i yok saymak) yırtık rapor testini kırıyor.

## B31 — el sıkışma accept döngüsünün dışında (`net/b31-handshake-off-accept`)

WS, TLS ve QUIC kapısı el sıkışmayı `accept()`'in içinde yapıyordu;
sunucunun accept döngüsü onları sırayla bekliyordu: yükseltme göndermeyen
TEK bir soket kapıyı 10 sn kilitliyor (20 istemcinin connect p50'si 9612
ms — hizmet engelleme), bağlanma fırtınasında backlog taşıyor, başarısız
el sıkışma döngüyü 100 ms geri çekiyordu (B29'un bulgusu).

- Her el sıkışan kapının kabul görevi ham bağlantıyı alır ve bağlantı
  başına bir el sıkışma görevi başlatır (tek await: kapı ⊃ 10 sn ⊃ el
  sıkışma); biten uç nokta kuyrukla `accept`'e gelir — `Listener::accept`'in
  şekli değişmedi (`gsb_net::transport::intake`).
- Uçuştaki el sıkışma kapı başına sunucunun unauthed cap'iyle sınırlı
  (vars. 25 000; yeni config anahtarı yok — el sıkışma aşaması sonraki
  aşamadan ucuza tüketilemesin); sınır üstündeki bağlantı hemen kapanır
  (QUIC: `refuse`) ve sayılır, kuyruğa girmez. Yuva, accept döngüsü uç
  noktayı alana dek tutulur.
- `close()` uçuştaki el sıkışmaları da keser (B16 sözleşmesi korunur,
  `StopReport` 0 abort).
- Sayaçlar: `Listener::handshake_stats()` (`in_flight/completed/refused/
  timed_out/failed`) + kabul görevinin kapanış özeti. Davranış değişikliği:
  başarısız el sıkışma artık accept hatası değil — sayılır ve warn basar.
- Ölçüm (release, önce/sonra dönüşümlü): sessiz soket + 20 WS istemcisi
  connect p50/p99 9612/9612 → 0/0 ms; orkestre 500 WS 1057/1457 → 34/1075
  ms (TCP değişmedi). Tel baytı değişmedi, yeni bağımlılık yok.
- Yan bulgu: `loadgen_games::loadgen_drives_the_mmo` CPU yükü altında
  kırılgan (`shard_members` toplamı N+1) — önceki kodda da (F18).

Testler 1049 → 1071 (+22); dokuzu eski kodda kırmızıydı; ajanın 13
mutasyonu yakalandı. Ebeveynin bağımsız mutasyonu (sınırı bir fazla
gevşetmek) 5 testi kırıyor.

## Küçük paket 3 — B6, B16, F15, F16, F17 (`core/small-bundle-3`)

- **B6 — aktörü ölmüş rUDP oturumu hemen gider** (DESIGN §6 "Aktörü ölmüş
  oturum"). Oturumu yazıcısı bırakır: aktörün posta kutusu kapalı ve
  güvenilir bandı borçsuzken (son bildirim ACK'lendi ya da REL canlılık
  sınırı vazgeçti) adresi sınırlı bir kuyruğa koyar ve demux'u kendi
  adresine bir baytlık datagramla uyandırır; demux her uyanışta kuyruğu
  boşaltır, oturumu yalnız gerçekten ölüyse siler. Sınır ~bir RTO (son
  bildirim ACK'lenmezse 5 sn + RTO); demux'a yeni await/zamanlayıcı/kilit
  yok.
- **B16 — accept döngüleri abort'suz biter** (DESIGN §9). Her ağaç içi
  listener accept'ini (el sıkışma dahil) bir `Door`'dan
  (`CancellationToken::run_until_cancelled`) geçirir; `close` bekleyen ve
  sonraki accept'leri `listener_closed()` ile bitirir, döngü döner.
  `stop()` döngüleri tek 1 sn'lik son tarih altında bekler, aşanı abort
  eder (geri sigorta) ve `StopReport { accept_loops_ended,
  accept_loops_aborted }` döndürür. Açık: HTTP ops döngüsü hâlâ abort ile
  (B33).
- **F15** — tıkalı bağlantının yanıtsız RPC retleri ayrı çekirdek sayaç:
  `requests_refused_congested` (satır `req_refused=`, Prometheus
  `gsb_room_requests_refused_congested_total`, loadgen teli **GSMD**,
  RESULT `req_refused=`); `req_rej_conn` artık yalnız yanıtlanan cap
  retleri.
- **F16** — ayrılma bekleme süresi ve RPC zaman aşımı tick saatinde
  (`ticker::now()`); üretim davranışı aynı; paused saatte grace/tavan ve
  zaman aşımı testleri.
- **F17** — mantık sayacı taşması görünür: `logic_counters_dropped=` ve
  `gsb_room_logic_counters_dropped` gauge'u yalnız sıfırdan büyükken (F9
  altın testi değişmeden geçer); `counters_dropped` adı ayrıldı.

Testler 1024 → 1049 (rebase sonrası); istemci baytı aynı. Ajanın
mutasyonları yakalandı; ebeveynin bağımsız mutasyonu (`is_listener_closed`
hep `false`) iki `accept_stop` testini kırıyor.

## B29 — loadgen WS modu; WS kapısının ilk yük ölçümü (`loadgen/b29-ws-mode`)

- `--transport tcp|udp|ws`: süreç içi ve `--serve` sunucusu `ws`'de tek
  `"ws"` dinleyicisi açar (TCP/rUDP değişmedi); `--addr` istemcileri yeni
  `gsb_client::connect::ws_stream` ile (çağıranın TCP soketi üstünde
  yükseltme); orkestratör iki çocuğa da iletir; churn, `--stall-ms`,
  `--capture`, `--flood-id` WS'de çalışır (flood artık maskeli mesaj
  yazıyor). `--tls-ca` + `ws` her modda kullanım hatası (kapının TLS
  biçimi yok).
- WS bayt muhasebesi: veri mesajı sokette durduğu boyla (başlık 2/4/10 +
  istemci yazımında 4 bayt maske + kare); yükseltme ve kontrol kareleri
  sayılmaz. TCP/rUDP sayımları ve RESULT satırları değişmedi.
- Ölçüm (release, 20 sn, TCP/WS dönüşümlü): demo 200/500, arena 200, MMO
  200 WS'de 30 Hz, hata/kapanış/düşme 0, adım süreleri gürültü içinde
  (MMO p90 +32–48 µs), sunucu bant genişliği aynı; istemci giriş baytında
  WS başlığı +%0,3–0,7, çıkışta +%38–50 (küçük girdi kareleri).
- **Bulgu B31 (düzeltilmedi, sıraya alındı):** WS kapısı yükseltmeyi
  `accept()` içinde yapıyor, el sıkışmalar SERİ — orkestre 500'de WS
  connect p50 ~1065 ms (TCP 17–20 ms); yükseltme göndermeyen TEK boşta
  soket kapıyı `WS_HANDSHAKE_TIMEOUT` (10 sn) kilitliyor (20 istemci p50
  = 9610 ms; TCP'de 0). TLS kapısı kod okumasına göre aynı yapıda.

Testler 1015 → 1024. Ajanın mutasyonları yakalandı; ebeveynin bağımsız
mutasyonu (WS 126 bayt sınırını kaydırmak) 2 testi kırıyor.

## F9 — oyunun/kit'in kendi metrik sayaçları (`core/f9-counter-seam`)

- **Seam (gsb-core):** `GameLogic::logic_counters(&self, world, out:
  &mut LogicCounters)` (varsayılan boş). Sayaç bir `const` bildirimdir —
  `LogicCounter::sum/max(ad, help)`; ad kuralı (1–32 bayt `[a-z0-9_]`,
  harfle başlar, `_total` ile bitmez) const değerlendirmede denetlenir.
  Değer mantığın kendi alanında (tick yolunda tahsis/kilit/mesaj yok),
  aktör örnek başına bir kez okur. Küme `RoomSample`/`RoomReport`'ta
  değerle (`Copy`, en çok 16 ad; fazlası atılır, sayılır, bir kez warn).
  Katlama kuralı sayaçla birlikte: SUM / MAX.
- **Görünüm:** `gsb-metric` satırında çekirdek anahtarlarından sonra
  `logic_<ad>=`; Prometheus'ta ad başına aile (`gsb_room_logic_<ad>_total`
  counter, MAX için gauge; `name` etiketli tek aile elendi — bir aile hem
  counter hem gauge olamaz); loadgen teli `GSMB` → `GSMC`; RESULT'ta
  `game=`'den önce genel `logic_<ad>=` segmenti. Sayaç bildirmeyen
  mantığın metni bayt bayt aynı (önceki koddan sabitlenen golden test).
- **Yeni bir oyun sayacı artık çekirdeğe, loadgen'e ve Prometheus koduna
  dokunmuyor:** oyunda bir `const` + bir alan + `counters`'ta bir `put`.
- **Kit:** `Game::counters` (varsayılan boş), yedi kit odası iletir.
  Crystallization'ı açan sharded oda altı `crystal_*` sayacı koyar
  (`moves`, `release_quiet/band/partner`, `untracked`, `fights_peak`
  MAX) — CROSS-SHARD §4c madde 5 kapandı. Doğrulama: savaş `war_kills`.
- 200 botluk 60 sn MMO düello koşusu: `logic_crystal_moves=18`, release
  quiet/band/partner 14/11/11, `fights_peak=9`.

Testler 988 → 1015; ajanın 59 mutasyonu yakalandı. Ebeveynin bağımsız
mutasyonu (MAX kuralını SUM gibi katlamak) 3 testi kırıyor.

## Küçük paket 2 — B24, B26, B27, F10, F13 (`misc/small-bundle-2`)

- **B24 (gsb-net):** WS kapısı, sunucunun bitirdiği oturumu artık boş
  kapanış çerçevesiyle (istemcide 1005) değil **1001 "Going Away"** ile
  kapatıyor (yalnız durum kodu: `88 02 03 E9`). Kapı sebebi bilmediği
  için bu, `stop()`'un yanında aktörün bitirdiği her oturumda (idle,
  bütçe, cap, supersede) gider; hüküm önündeki ERROR karesinde kalır.
  Okuyucunun hata kapanışları (1002/1003/1007/1009) ve istemci
  kapanışının yankısı değişmedi. Yan düzeltme: sunucu önce kapattığında
  istemcinin cevap kapanışı yine yankılanıyor, istemci İKİNCİ bir
  kapanış çerçevesi alıyordu (RFC 6455 §5.5.1 ihlali); yazıcı artık ilk
  kapanıştan sonra hiçbir çerçeve yazmıyor. Ebeveyn, B25'in stop testini
  `Some(1001)`'e sıkılaştırdı.
- **B26 (loadgen):** churn istemcisi baytları `run_client`'ın kuralıyla
  sayıyor (akışta `4+2+yük`, rUDP'de datagram) — AUTH gerçek boyuyla, her
  JOIN denemesi, girdiler, JOIN evresinin bütün cevapları. `run_client`
  akışın EOF'unda oturumu bitiriyor (eskiden hamle yazımı hata verene
  dek boş dönüyordu). RESULT'ta BİLEREK oynayan: churn modunda
  `client_out_bps` (TCP +%36, rUDP +%7–8); `client_in_bps` kare başına
  +4/+1 bayt (gürültü içinde); düz koşuda hiçbir alan.
- **B27 (loadgen):** `--tls-ca` koşu başında bir kez okunuyor; bütün
  bağlantılar tek rustls istemci yapılandırmasını paylaşıyor.
- **F10 (gsb-core):** ticker runtime saatinde (`tokio::time::Instant`)
  zamanlıyor ve damgalıyor; üretimde aynı an, duraklatılmış test
  saatinde `dt` = periyot. Damgayla karşılaştırılan okumalar
  (`late_us`, girdi-boşta saatinin katılım damgaları) `ticker::now()`'a;
  adım süresi duvar saatinde (CPU işi). Cephe'nin yürüyen senaryosu
  paused saatte: 2,04 sn → ~0,05 sn, aynı tick'ler.
- **F13 (gsb-kit):** `record_appearance`'ın bekleyen değişikliği silmesi
  kodda zaten vardı; A10'un sağ kalan mutasyonunu öldüren test eklendi.

Testler 979 → 988 (rebase sonrası). Ajanın mutasyonları yakalandı;
ebeveynin bağımsız mutasyonu (kapanış kodunu 1000 yapmak) 4 testi
kırıyor.

## B25 — `gsb-client`'e WebSocket yarısı (`client/b25-ws-half`)

B19 WS yarısını bilerek dışarıda bırakmıştı; sunucu testlerinin WS
istemcileri el yazmasıydı (kendi el sıkışmaları, sabit maske anahtarı,
`timeout` altında `read_exact` — B19'un iptal-güvensizlik hatası)
(DESIGN §5.7).

- Yeni `Conn` varyantı yok: WS bağlantısı aynı `Conn::Stream` (kapı her
  ikili mesajda tam bir akış-teli karesi taşır); `FrameRx`/`FrameTx`
  altında WS konuşur. Kapanış kodu/nedeni `Conn::ws_close()` →
  `WsClose { code: Option<u16>, reason }`; `Recv::Closed` birim kalır.
- Açıcılar `connect::ws(addr)` (düz `ws://`), `ws::handshake(io, host,
  path)` (her akış üstünde; gsb kapısının TLS biçimi yok, `wss://` gsb'ye
  karşı sınanmadı). El sıkışma: OS-rastgele anahtar, accept denetimi,
  istenmemiş alt protokol/uzantı reddi.
- Her istemci çerçevesi taze OS-rastgele anahtarla maskeli; parçalı
  mesajlar birleşir; ping'e pong; kapanış tek yankı; sonrasında gönderim
  `BrokenPipe`, gelen bayt `InvalidData`; koruma kareninkiyle aynı;
  iptal güvenliği iki yönde.
- Yeni crate yok (`sha1`, `getrandom` çalışma alanında; `base64` 0.22
  kilitte zaten vardı, çalışma alanı bağımlılığı oldu).
- Göç: multi_listener, stop_notice/client.rs, stream_rejected; iddialar
  ve test sayıları aynı. Kalan: `gsb-net` ws süitinin sahte istemcisi
  (bağımlılık döngüsü; bilerek bozuk bayt yazar). Sunucu davranışı ve
  baytları değişmedi.

Testler 952 → 979 (+23 WS birim, +4 `ws_client.rs` gerçek kapı); her kural
mutasyonla kırıldı. Ebeveynin bağımsız mutasyonu (el sıkışmada
`Sec-WebSocket-Accept` denetimini kapatmak) el sıkışma testini kırıyor.

## B19 — istemci yapı taşı `gsb-client` (`client/b19-gsb-client`)

Sunucunun istemci yarısı bir yapı taşı değil, kopyaydı: çerçeve
okuyucu/yazıcı ve oturum adımları loadgen'de, örnek istemcide ve on bir
sunucu test dosyasında ayrı ayrı yazılmıştı; TLS bağlayıcı üç, QUIC
istemci kurulumu üç kez (DESIGN §5.7).

- Yeni motor crate'i `gsb-client` (oyun bilmez, politika taşımaz):
  `frame` (`encode`; iptal-güvenli `FrameRx` — 4 MiB koruma, kare içi EOF
  `UnexpectedEof`; `FrameTx`), tek `Conn` (TCP / TLS / QUIC bi-stream /
  rUDP; `send`, `send_batch`, sınırlı `recv`, `into_split`), açıcılar
  (`connect`, `tls`, `quic` — güven kökleri çağırandan), `session` (AUTH
  ± bilet, JOIN, HEARTBEAT, LEAVE; `Credentials` = resume anahtarı;
  bekleme sırasında gelen diğer kareler sırayla çağırana), tipli
  `ServerError` (`ErrorCode` + ham numara + mesaj; bilinmeyen kod
  `Unspecified`). Yeni üçüncü taraf bağımlılık yok.
- Bulunan hata: her kopya `read_exact`'i `timeout` ile sarıyordu —
  pencere önek ile gövde arasında dolarsa önek kayboluyor, akış
  kayıyordu. Yeni okuyucu yarım kareyi tamponda tutar. Kopyaların
  bazılarında boyut koruması ve AUTH sonucu denetimi de yoktu.
- Göç: loadgen (`wire.rs` silindi), örnek istemci ve on üç sunucu test
  dosyası. Kalanlar: WS istemcileri (`gsb-client`'te WS yarısı yok;
  `gsb-net` döngü yüzünden kullanamaz), `udp_rel_liveness`'ın doğrudan
  `UdpClient` sürüşü, e2e koruma akışlarının adım adım döngüleri.
- Tel baytları değişmedi (eski kodlayıcılar donmuş kopya olarak `encode`
  ile bayt bayt eşit; silinmeden önce HEAD'deki yardımcılarla 94 kare /
  790 017 baytta kodlama ve okuma özdeş); loadgen RESULT biçimi aynı.
  Loadgen'in RESULT değerlerini oynatmamak için korunan tuhaflıklar
  BACKLOG B26/B27'de.

Testler 928 → 952 (rebase sonrası; +17 `gsb-client` birim, +7
`gsb-server/tests/client_session.rs` gerçek sunucu); her kural mutasyonla
kırıldı. Ebeveynin bağımsız mutasyonu (uzunluk önekine opcode'un 2
baytını katmamak) 13 testi kırıyor. Not: ajan kendi push edilmemiş,
derlenmeyen ilk commit'ini `reset --soft` ile geri alıp doğrusunu yazdı
(kural sapması, kayıp yok).

## A29 — takım bütçesinde oyunun sıralaması; kesme barındırıcıya (`kit/a29-team-budget-priority`)

W2-3'ün iki eksiği: bütçe kestiğinde her kademede kalanlar kit'in
karşılaşma sırasıydı (Cephe'de önce doğan kuleler hep kalır, en yeni
oyuncular gider) ve kit'in `over_budget` sayacı barındırıcıya
ulaşmıyordu.

- Opt-in `ShardedTeamRoom::with_export_rank(fn(&Wire<G>) -> u32)`: yüksek
  sıra önce, eşitlikte küçük wire id (şeridin sırasından bağımsız).
  Sıralama "önce üyeler"in yerine geçmez, kademe içinde inceltir
  (üyelik yalnız kit'te bilinir; ödünç kaydın takımı yok). Kalanlar
  export'un kendi sırasında gider. Sıralama yoksa kesme A29 öncesinin
  birebir aynısı (tohumlu kalabalıkta export özeti sabitli).
- Maliyet: bütçe aşılmazsa hiçbir şey; aşılırsa kesilen kademede
  doğrusal seçim (`select_nth_unstable_by`) — takım başına O(n), sort
  yok, tahsis yok.
- Kesme export'la çekirdeğe: `TeamExport::over_budget` (röle edilmez) →
  `RoomSample::team_over_budget`, `gsb-metric` satırı, Prometheus
  `gsb_room_team_over_budget_total`, loadgen teli **`GSMB`**, savaş
  RESULT'ında `team_over_budget`.
- Savaş demosuna dokunulmadı (varsayılan bütçede kesme yok; sıralama
  oyunun kararı); seam'i kit'in fixture oyunu doğruladı.

Testler 920 → 928; ajanın rank ve sayaç mutasyonlarının hepsi yakalandı.
Ebeveynin bağımsız mutasyonu (ikinci kademenin payından üyeleri düşmemek
— kesme bütçeyi aşar) 4 testi kırıyor.

## B12 + B13 — kapanış bildirimleri: sunucu durdurma ERROR 14, reddedilen akış ERROR 9 (`core/b12-b13-close-notices`)

Sunucunun kendi başlattığı iki kapanış istemciye sessizdi: `stop()`
(aktörün `Shutdown` kolu yalnız döngüden çıkıyordu) ve taşımanın bayt
akışını reddetmesi (`stream_rejected`). İstemci "sunucu gidiyor" ile "ağ
koptu"yu ayıramıyordu (DESIGN §5.6).

- Yeni kod `ERROR_CODE_SERVER_STOPPING = 14` (toplamalı; `PROTOCOL_VERSION`
  artmadı — tanımayan istemci OTHER sayar, kapanışı soketten öğrenir).
  Anlamı: oturum hakkında hüküm yok, bu adreste resume yok; sonra ya da
  başka sunucuya bağlan (ne zaman/nereye — kontrol düzlemi politikası,
  motor değil). Reddedilen akış mevcut ERROR 9'u alır: `stream rejected:
  <sebep>`.
- İkisi de en-iyi-çaba ve beklemesiz: çıkış kuyruğuna senkron `try_send`
  (`conn/actor/close.rs::try_notice`); kuyruk doluysa bildirim düşer, aktör
  yine anında çıkar. `stop()`'un await'leri ve S'nin kuralları değişmedi.
- QUIC: dinleyici `close`'u artık endpoint'i kapatmıyor
  (`set_server_config(None)`; `Endpoint::close` her bildirimi terk
  ediyordu); gönderme yarısının kapanışı eşin ACK'ini bekliyor
  (`quic/send.rs`; stall penceresi / quinn idle ile sınırlı).
- WS: soket yazıcı görevi kapanış çerçevesinden sonra veri çerçevesi
  yazmıyor (RFC 6455 §5.5.1) — önceden fan-out da oraya düşebiliyordu.
- Kapı tablosu (DESIGN §5.6): TCP/TLS/QUIC bildirim → son; WS bildirim →
  kapanış çerçevesi; rUDP'de FIN yok, bildirim tek kapanış sinyali; bozuk
  TLS kaydında bildirim inemez (oturum ölü).
- B14 yapılmadı: kod-9 mesajı sözleşmece insan-okunur; sunucu
  `server_closes{reason}` otorite ve loadgen onu basıyor.

Testler 905 → 920 (rebase sonrası; +4 aktör, +6 kapı başına durdurma,
+4 reddedilen akış, +1 WS `after_close`); her kural mutasyonla kırıldı.
Ebeveynin bağımsız mutasyonu (reddedilen akışa 9 yerine 14 göndermek) 4
testi kırıyor. Resume semantiği değişmedi.

## F14 — düşen batch'in RPC yanıtları kaybolmuyor (`core/f14-rpc-replies-on-drop`)

Çekirdeğin kendi yükü, F11'in çekirdek tarafı; hiçbir şey düşmediğinde
bayt ve yol aynı (RPC-CONTROL-PLANE §3.1, KIT-ARCHITECTURE §10 "F11" F11-1).

- Kural: fan-out (oda 4d / shard 6d) bir batch'i düşürünce, `private`'a
  verilen RPC yanıtları bağlantının `queued` kuyruğunun başına geri
  konuyor ve kanalın kabul ettiği ilk batch'le — sonraki yanıtlardan
  önce, tam bir kez — gidiyor. Yeni kanal/kilit/await/alan yok. Batch'te
  çekirdeğe ait başka yük yok (kontrol/hata kareleri fan-out'a binmiyor).
- Fırtına sınırı: tıkalı bağlantı (`RoomConn.dropping`) borcu — kuyruk +
  taşınan + uçuştaki — `max_pending_requests_per_conn`'a ulaşınca yeni
  isteği (bozuk zarf dahil) işlemeden ve yanıtlamadan reddediyor
  (`requests_rejected_conn_cap`; hiçbir şey uygulanmadığı için istemcinin
  zaman aşımından sonra yeniden denemesi güvenli). Kesin sınır: bağlantı
  başına borç ≤ cap + bağlantı başı tick çekimi (varsayılan 4 + 16 = 20).
  Tıkalı olmayan bağlantı hiç reddedilmez.
- Garanti artık: kabul edilmiş isteğin yanıtı bağlantı boşaldığı anda,
  tam bir kez ve sırayla gider. İstemci zaman aşımına kalanlar: oturumu
  önce biten bağlantı (leave, detach — write-stall kapanışı dahil —, göç);
  fırtına sınırında reddedilen istek; `responses`'ı kodlamayan mantık.

Testler 893 → 905 (+7 oda, +5 shard); önce kırmızı, ajanın 9 mutasyonu
iki aktörde de yakalandı. Ebeveynin bağımsız mutasyonu (geri konan
yanıtların sırasını ters çevirmek) 2 testi kırıyor.

## F11 — fan-out düşme sinyali: tek seferlik durumun yeniden kurulması (`core/f11-drop-signal`)

Çekirdeğe iki kanca, kit'e cevabı; hiçbir şey düşmediğinde bayt aynı,
istemci kuralı aynı (KIT-ARCHITECTURE §10 "F11").

- Seam: `GameLogic::on_batch_dropped(world, player, snapshot)` ve
  `GameLogic::on_batch_resumed(world, player)` — varsayılan no-op; oda ve
  shard aktörü aynı noktada çağırıyor: fan-out yinelemesinde, o oyuncunun
  `private`'ının hemen ardından (senkron, kanal/await yok; satırda tek
  `bool`). `resumed` = düşme dizisinden sonra kanalın kabul ettiği ilk batch.
- Kit: `InputSeq`'in tek "taşınan" yuvası düşen karenin ack'ini ve oturum
  yükünü (`Welcome`) yeniden borçlandırıyor (göçte de taşınıyor —
  `ShardInputRecord.greet`); `Baselines` görünüm içeriği (grup karesi ya da
  one-shot full) taşıyan batch düşünce tabanı geri alıyor → sonraki tick
  one-shot full (kaybolan `removed` hayaletini de düzeltir). AOI odaları
  `conn_view` yerine `Baselines` kullanıyor (bayt aynı).
- Fırtına sınırı: hiçbir şey geçmezken yeniden gönderimler 1, 2, 4, 8, 16,
  sonra 32 adım arayla; kanal yeniden kabul edince bekleme kalkıyor (kabul
  edilen batch başına en çok bir yeniden gönderim; oyuncu başına tek
  bekleyen). Resume sinyalsiz ilk tasarım ölçümde düştü (tempo 32'ye
  tırmanınca iyileşme keep-alive kadar gecikiyordu).
- Ölçüm (spatial 200, 10 sn duraklamalar, `--conn-out 4`, dönüşümlü):
  düşen batch'lerden sonra görünümün iyileşmesi 17,6 tick / 587 ms →
  0,9–1,5 tick / 31–32 ms; errors=0; varsayılan arena koşusu aynı.
- Loadgen koşum düğmeleri: `--stall-ms`/`--stall-every-ms` (yavaş okuyucu;
  loopback'te düşme görmek için 1200 MSS kelepçesi — `socket2` artık
  `gsb-server`'ın doğrudan bağımlılığı, lock'ta zaten vardı) ve
  `--conn-out N`.
- Bulgu F11-1: düşen batch'teki RPC yanıtları kayboluyor → BACKLOG F14.

Testler 869 → 893 (rebase sonrası; +4 çekirdek, +17 kit, +3 loadgen);
her kural mutasyonla kırıldı; hiçbir bayt kilidine dokunulmadı. Ebeveynin
bağımsız mutasyonu (görünüm içeriği taşımayan düşmede de tabanı geri
almak) `Baselines` testini kırıyor.

## F8 — admin `POST /rooms/open` sunucunun odasını açar (`server/f8-rooms-open-config`)

Runtime'da açılan oda eskiden `RoomConfig::default()` + `tick_hz` ile
kuruluyordu: sunucunun oda anahtarlarını (`room_control`, `conn_action`,
`max_snapshot_bytes`, `keepalive_hz`, `max_players`, `max_idle_input_secs`,
`max_detach_hold_secs`) almıyordu — aynı sunucunun runtime odası başka
kapasitelerle ve hep 10 dk'lık detach-hold tavanıyla çalışıyordu; oda
anahtarı yazılmış bir sunucuda ön-kurulan odayı yeniden açmak 409
dönüyordu.

- Tek eşleme: `Config::room_template` (`RoomTemplate`, yalnız `Config`
  üretir; `config/axes/listeners/room.rs`). `Config::room_config` ve ops
  yüzeyi ondan kurar; yüzey kendi varsayılanını uyduramaz. Çekirdek
  değişmedi.
- `tick_hz` istek başına tek geçersiz kılma (doğrulama aynı). Başka
  geçersiz kılma eklenmedi: güvenlik tavanları operatör politikası,
  kimliksiz v1 yüzeyi onları oda başına gevşetemesin.
- Oyun modülü ayarları zaten doğruydu (fabrika her `CreateRoom`'da aynı).
- Sözleşme: kodlar/gövdeler/idempotentlik aynı; sonuç olarak ön-kurulan
  odayı aynı hızla yeniden açmak artık 200, `keepalive_hz`'ten küçük
  `tick_hz` isteği sunucunun `keepalive_hz`'ine göre 400 (OPS §2).

Testler 865 → 869 (birim: registry'ye giden istek `room_config(id)` ile
alan alanına aynı; geçersiz hız registry'ye gitmez; HTTP; MMO uçtan uca —
düzeltmeden önce runtime odada dövüşçü 6,03 sn tutuluyordu, tavan 1 sn).
Ajanın 6 mutasyonu yakalandı; ebeveynin bağımsız mutasyonu (şablondan
`max_snapshot_bytes` eşlemesini düşürmek) birim testini kırıyor.

## A10 — kayıt başına yayın hızı: oyun başına opt-in (`kit/a10-send-rate`)

Kit'e yapı taşı; varsayılan değişmedi, istemci kuralı değişmedi (`kit.proto`
aynı — kayıtlar hâlâ mutlak) (KIT-ARCHITECTURE §10 "A10").

- Seam: `RecordCodec::send_every(&wire) -> SendEvery` (varsayılan `Tick`;
  `Ticks2/4/8/16`), takvim `SendEvery::due(step, wire)` public. Sınıf
  kaydın wire değerinden (ödünç kayıtta da hesaplanabilsin diye); sınıflar
  2'nin kuvvetleri (iç içe vadeler — sınıf değişse de sınır tutar); faz
  wire id'nin Fibonacci karmasından (A30'un iç içe id'leriyle `id mod`
  aynı adıma yığardı); saat odanın adım sayacı.
- Motor garantisi: değişen ama vadesiz kayıt tutulur ve vadesinde GÜNCEL
  değeriyle gider — bayatlık ≤ `ticks() − 1` adım (ithal kayıtta +1
  röle). Görünüme giriş, hücre geçişi, göç varışı, `removed`,
  `cell_exits` ve her full (taze, keep-alive, one-shot) beklemez;
  full-only odalar hızı yok sayar. Delta motorları (set defteri, hücre
  defteri, ödünç şerit, sharded takım ihracı) kapsandı; encode-once
  korunuyor; A31 koşusuyla çalışıyor.
- Varsayılan bayt birebir: dokuz tohumlu ikiz oturumun içerik özeti
  değişiklikten ÖNCE sabitlendi, sonra aynı.
- Doğrulama: yalnız arena açtı (`Ticks2`, 15 Hz): istemci başına bant
  200'de −43,1 %, 500'de −44,7 % (rUDP −43,8 %); rUDP'de kare başına
  datagram 2,9 → 1,9; capture'dan bayatlık tam sınırda (1 tick). Arenanın
  iki testi bilinçli çevrildi (bir tick tolerans).

Testler 847 → 865. Ajanın 14 mutasyonu yakalandı (biri — görünüme girişte
bekleyen kaydı silmemek — sağ kalıyor, etkisi fazladan bir idempotent
upsert: F13). Ebeveynin bağımsız mutasyonu (her sınıfın periyodunu iki
katına çıkarmak) 8 testi kırıyor.

## A31 — paketli kayıt koşusu: oyun başına opt-in (`kit/a31-packed-records`)

Kit'e yapı taşı; varsayılan değişmedi (KIT-ARCHITECTURE §10 "A31").

- `RecordCodec::RUN` (vars. `false`): açan oyunun kayıtları tek bir
  `bytes records = 6` alanında art arda (`id varint + oyunun kendini
  sınırlayan gövdesi` — gövde formatı tamamen oyunun: protobuf,
  MessagePack, bit paketli…); kayıt başına çerçeve yok. İstemci:
  `ClientDecoder::RUN` + `run_record(id, run)`, yeni
  `ClientError::UnexpectedRun`; `client::wire::varint` public.
  `kit.proto`'ya alan ve istemci kuralı eklendi (additive; framing oyunun
  protokol sürümünün parçası, müzakere yok).
- Bütün yazıcı yolları tek bölgeden (açık/PVS/düz sharded, takım ve
  sharded takım defteri, ödünç ve ithal kayıtlar, AOI ve sharded ×
  spatial parçaları; full/delta/keep-alive/one-shot). Açmayan oyunun
  baytı birebir aynı (oda türü başına ikiz testler + mevcut bayt pinleri).
- Sınır kararı: kendini sınırlayan gövde; kit uzunluk öneki elendi
  (savaş baytının %12,6–12,7'si — isteyen oyun kendi önekini yazar).
- Doğrulama: yalnız savaş demosu açtı (6 B'lik gövde). `out_bps_per_conn`
  200 / 500: −53,9 / −54,1 % (TCP), 500 rUDP −52,9 %; 500'de kare başına
  datagram 3,9–4,0 → 2,0–2,1. Diğer demolar dokunulmadı (üç wire testine
  yalnız yeni alan için `records: Vec::new()` eklendi; pinli baytlar aynı).
- Bulgular: A31-1 shard'lı iki aktör odası aynı baytı göndermez (seam
  zamanlaması) — bayt karşılaştıran testler elle adımlanır; A31-2 rUDP
  yazıcısının parça sayaçları loadgen RESULT'ında yok; A31-3 500'de
  kareler hâlâ parçalanıyor → A10.

Testler 827 → 847. Ajanın 12 kural mutasyonu yakalandı; ebeveynin bağımsız
mutasyonu (tam 128 baytlık koşunun uzunluğunu tek bayta yazmak) 6 ikiz
testini kırıyor.

## A30 — kompakt wire id: iç içe basım (`core/a30-compact-ids`)

Motor geneli varsayılan; zarf düzeni ve istemci kuralları aynı, yalnız
id değerleri değişti (KIT-ARCHITECTURE §10 "A30").

- `N` shard'lı odada shard `i`'nin `n`'inci çekimi `(n − 1)·N + i + 1`
  (`gsb_core::shard::interleaved_id`; tersi `minting_shard`); kit
  `Minter::Range` → `Minter::Interleaved`. `SHARD_SERIAL_RANGE` kalktı →
  `SHARD_SERIAL_CAPACITY` (aynı 2^20; shard başına çekim sınırı, aynı
  `RoomFull` yolu); `ShardLogic::serial_base` kalktı, `serial_range` →
  `serial_capacity`.
- Değişmezler korunuyor (tekil, enkarnasyonda yeniden kullanılmaz, göçte
  korunur, koordinasyonsuz, deterministik sıra); tekillik artık sınıra
  dayanmıyor (eskiden korumasız orphan damgalaması komşu aralığa
  taşabilirdi). Kristalleşmede "yüksek wire" artık kabaca "daha geç
  çekilen" (kural aynı, deterministik).
- Ölçüm (`f59d432`'ye karşı dönüşümlü çiftler, `out_bps_per_conn`, 200 /
  500): demo −15,4 / −9,0 %, MMO −8,6 / −7,1 %, savaş −6,7 / −7,8 %;
  arena (tek oda) değişmedi. Ortalama id varint 2,6–3,2 B → 1,7–1,9 B.
- Hiçbir bayt sabitleme testi değişmedi.

Testler 821 → 827. Ajanın 5 değişmez mutasyonu yakalandı; ebeveynin
bağımsız mutasyonu (her shard'ı tek shard'lı oda gibi basmak) kit, MMO ve
savaşta 8'den fazla testi kırıyor.

## A22 faz 0 — değer düzeyinde delta: ölçüm ve tasarım (`kit/a22-value-delta`)

Wire değişmedi (KIT-ARCHITECTURE §10 "A22 — değer düzeyinde delta").
Tek kod: `gsb-loadgen --capture DIR [--capture-clients K]` — örneklenen
istemcilerin oyun bandı kareleri + JOIN sonucu `client-<id>.gsbcap`'e
(ölçüm aracı; orkestre/serve/churn reddedilir; yakalanan dosya
`ClientView`'dan geçince istemcinin sayaçlarını birebir veriyor — test).

- **Bayt anatomisi** (dört oyun, 200/500): bandın %87–96'sı grup
  delta'ları; kayıt başına çerçeve (tag + uzunluk) %11–20, wire id
  %19–40 (shard aralığı `k·2^20` → 3–4 B varint), konum %33–64, diğer
  alanlar MMO/savaşta %25–33. Delta kayıtlarının %99'u yalnız konum
  değiştiriyor ve tick başına değişim hep 1 zigzag bayta sığıyor.
- **Yeniden oynatım** (500, bugüne göre): yalnız sıkı oyun kodeği
  −25…−38 %; + paketli kayıt koşusu + kompakt id −43…−58 %; değer-göreli
  kodlamanın bunun ÜSTÜNE katkısı −2…+8 puan (MMO +23); bağlantı başına
  onaylı baseline 1000'de tick bütçesini aşıyor (arena ~35 ms/tick);
  A10 (15 Hz) + mutlak sıkıştırma −59…−74 %.
- **rUDP kayıp simülasyonu:** göreli seçenekler bayat görünüm oranını
  5–10 kat artırıyor (baseline denetimsiz göreli: sessizce YANLIŞ
  değer); küçük kareler ise parçalanmayı azalttığı için bugünkü kurallarda
  bayatlığı yarıya indiriyor.
- **Öneri:** değer-göreli kodlama şimdilik yapılmayacak; kaldıraç sırası
  kompakt wire id → paketli kayıt koşusu + oyun kodeği → A10; A22 ancak
  bunlardan sonra hâlâ baskı gören AOI tipi bir oyun için (adlandırılmış
  baseline + resync). Beş bakımcı sorusu açık (BACKLOG E7).
- **Yan bulgu (A22-1):** fan-out düşmesi one-shot full içeren batch'i
  atınca `Baselines` istemciyi baseline'lı sayıyor — bugün zararsız
  (keep-alive iyileştirir), herhangi bir göreli şemada doğruluk hatası
  (BACKLOG F11).

Testler 818 → 821. Çözümleyici (~1 850 satır) repoda değil.

## W2 turu — doğrulama oyunu "Cephe" (`demo/w2-war`)

Kit'in dördüncü doğrulama oyunu ve W1'in `team × sharded` kompozitinin ilk
kullanıcısı (KIT-ARCHITECTURE §10 "W2 sonucu", GAME-MODULE "W2 sonucu",
CROSS-SHARD §8b.8): üç fraksiyon, 1 600 m'lik haritada 2×2 shard, harita
genelinde takım sisi — müttefik her yerde, düşman yalnız fraksiyonun bir
birimi (oyuncu ya da her bölgedeki gözcü kulesi) onu görürken, başka shard'da
bile. **Kit'e dokunulmadı.**

- `gsb-demo-war`: `ShardedTeamRoom` + `VisionGrid2` (60 m) + köşegenli
  `GridPartition2`, delta modu; 12 gözcü kulesi (fraksiyon başına bölge
  başına bir), iki ele geçirme noktası (W1 nötr kuralı; ele geçirilen nokta
  fraksiyonun birimi olur); kayıtlı karakter (fraksiyon + konum) doğrulanmış
  kimlikle, kaydı olmayana kimliğin FNV-1a özetiyle fraksiyon ve üs;
  `Welcome { faction, factions }` `Private.game`'de; seam ötesi yakın dövüş
  MMO'nun uzak etki kalıbıyla, öldürme sahibin shard'ında bir kez;
  `war.proto`, kayıtta 1 tabanlı fraksiyon.
- Sunucu: `game = "war"`, özellik `game-war` (varsayılan açık; CI'ın oyunsuz
  işi tek başına derler/lint'ler); `[war]` = `disconnect_grace_secs`,
  `team_budget`; sabit anahtarlar reddedilir.
- Çekirdek (A26 kapandı): takım sayaçları `RoomSample::team_*` (yedi alan)
  → rapor, `gsb-metric`, Prometheus `gsb_room_team_*_total`; loadgen metrik
  teli `GSMA`.
- Loadgen `--game war`: kadro `lg-{id}` (fraksiyon `id mod 3`), fraksiyonu
  `Welcome`'dan alır, karakollar arasında dolaşır (orta nokta seam'leri
  keser), menzildeki düşmana saldırır; RESULT `shard_members=` + `team_*`.
- **Ölçüm** (release, `--duration 10 --write-stall-secs 0`, yük 28–44):
  200/500/1000 (orkestre) `errors=0`, 30 Hz; 1000'de adım p50/p90
  1,4–1,7/2,4–3,1 ms, `out_bps_per_conn` ~365 KB/sn (istemci başına ~956
  birim — müttefiğin harita geneli görünmesi senaryonun doğası, röle
  değil); röle 120 export/sn, export başına ≈ 0,78·N kayıt, yayılım 3,00,
  düşme/tavan/TTL 0. rUDP 500: `frag_reassembled` 129 k, `frag_dropped` 0.
  A25 (export temposu) gerekmedi; A13 tetiksiz.
- **Bulgular (kod değişmedi):** W2-1 nötr kuralı bölüm artefaktı üretiyor
  (shard 3'teki oyuncu 640 m ötedeki sahipsiz noktayı görüyor, 100 m'deki
  shard 0 oyuncusu görmüyor); W2-2 birim başına yarıçap gerekmedi; W2-3
  bütçe üyeleri wire sırasıyla kesiyor (kuleler kalır, en yeni oyuncular
  gider) ve kit'in `over_budget`'ı barındırıcıya ulaşmıyor; W2-4 `Ticker`
  std saatle damgalıyor — duraklatılmış test saatinde yürüyüş duruyor.

Testler 776 → 818 (+42). Ajanın mutasyonları: savaşın 22 kural mutantının
19'u tek başına kırıldı (seam ötesi taraf/menzil denetiminin iki katmanı
birbirinin yedeği, birlikte kırılıyor), sunucu modülü 5/5, sayaç yolu
5/5. Ebeveyn: iki düzeltme commit'ini (rustdoc bağı, kararsız loadgen
penceresi) ait oldukları commit'lere katladı — her commit tek başına
geçiyor (ilk 802, loadgen commit'i 818, doc kapısı temiz); bağımsız
mutasyonu (kayıtlı karakteri yok saymak) 7 testi kırıyor.

## W1 turu — team × sharded kompoziti (`kit/w1-team-sharded`)

Takım sisi artık shard'lı haritada (CROSS-SHARD §8b; BACKLOG §1 satır 6'nın
ilk yarısı). Her shard, her takım için o takımın BURADAKİ görünür kümesini
(üyeler + kendi birimlerinin gördüğü düşman/nötr — kendi ve ödünç kayıt)
oyunun kodlayıcısıyla bayt olarak registry'ye export eder; registry'nin oda
girdisindeki hub, export'u o takımı görüntüleyen DİĞER shard'lara `try_send`
ile röle eder (kayıt saklamaz). Alıcı, kaynak başına yuvayı wholesale
değiştirir, 64 tick sessiz kalan kaynağı düşürür, takım başına birleştirir.
§8'den en büyük sapma: yalnız üyeler değil GÖRÜNÜR KÜME — senaryo ("Cephe":
uzak bir müttefiğin gördüğü düşmanı başka shard'daki oyuncu da görür)
bunu gerektiriyor; bütçeli (takım başına tick'te 1024, üyeler önce).

- Çekirdek: `ShardLogic::team_exchange` (varsayılan `None`; `GameLogic`
  değişmedi — §8'in "ikinci slice"ı yerine), faz 5b TEAMS,
  `ShardMsg::TeamImport`, `RegistryMsg::TeamExport` (monomorfik, nesil
  denetimli), `TeamImports`; tavanlar 16 384 kayıt / 256 takım, TTL 64.
  Sayaçlar log satırı (`team_exchange_summary`, `team_hub_summary`).
- Kit: `ShardedTeamRoom<G, P, V>` (`with_shard(inner, vision, lent_pos)`,
  `with_delta`, `with_team_budget`), `TeamMig`; bir wire bir kez, yük
  önceliği yerel > ödünç > ithal. K4 kalıntısı:
  `TeamGame::spawn_team_player_as`, `TeamRoom` kimliği iletir. Kit'e
  dev-dependency `tokio` (`test-util`, duraklatılmış saatli aktör
  testleri için).
- Mevcut hiçbir oyunun istemci baytı değişmedi; MMO ve demo sharded
  loadgen 200 dönüşümlü A/B'de gürültü içinde.

Testler 740 → 776 (çekirdek 17, kit 19 — 8'i gerçek registry + dört shard
aktörüyle; 30/30 tekrar yeşil). Önce kırılan: `team_exchange` `None` → 8
gerçek aktör testinin 8'i. 17 çekirdek + 16 kit mutasyonu yakalandı.
Ebeveynin bağımsız mutasyonu (hub'ın takım filtresini kaldırmak) 2
çekirdek testini kırıyor; kit'in uçtan uca izolasyonu yine tutuyor
(alıcı da yalnız görüntülediği takımı birleştiriyor — iki katmanlı
izolasyon).

## T turu — takım odasında delta (`kit/t-team-delta`)

- **Kit:** `TeamRoom::with_delta` — takım sisi odası AOI'nin zarfı ve
  istemci kurallarıyla delta gönderebiliyor: takım başına `removed`
  (görünümden çıkan wire id'ler) + upsert (giren ya da wire değeri
  değişen kayıtlar), takım başına bir kez kodlanır; taze takıma,
  keep-alive'da ve baseline'ı olmayan üyeye (oynayan takıma katılım,
  resume, takım değişimi) one-shot full. `cell_exits` yok (görüş kümesi
  hücre birleşimi değil; bir hücre takım görüşüne yarı girebilir).
  Delta son GÖNDERİLENE göre (önceki tick'e değil). `ClientView` ve
  istemciler değişmedi. Varsayılan full mod bayt bayt aynı (eski
  kodlayıcıya karşı rastgele koşu + bir literal kare kilitli).
- **Ortak motor:** `common::SetLedger` (küme içerikli delta defteri) ve
  `common::Baselines`; üç delta odasının one-shot private full karesi
  tek yazıcı (`common::emit_private_full` — AOI ve sharded × spatial'daki
  kopyalar kalktı, bayt birebir). `all`/`pvs` adopsiyonu artık oda başına
  ~30 satır (BACKLOG A1).
- **Arena:** delta modunda (turun kasıtlı wire değişikliği).
- **Bulgu — kazanç küçük:** A/B (dönüşümlü, yük 19–46): bağlantı başı
  çıkış 200'de −%10, 500'de −%10, orkestre 1000'de −%9,5; rUDP 500'de
  parçalanan mesaj −%0,7. Görünür birimlerin ~%85–90'ı her tick
  santimetre konumunu değiştiriyor → kayıt başına delta neredeyse her
  kaydı yeniden yolluyor (yarısı duran kit koşusunda delta baytı
  full'un %38'i). Daha fazlası değer düzeyinde delta ister (BACKLOG
  A22). İstemci CPU'su orkestre 1000'de +%15 (delta uygulaması).
- Testler 722 → 740 (+18; rastgele 900 tick'lik yakınsama testi dahil).
  Ajanın 17 mutasyonu yakalandı; ebeveynin bağımsız mutasyonu (değişen
  kaydı hiç yeniden göndermemek) 5 kit + 2 arena testini kırıyor.

## S turu — çekirdek kapanış kilitlenmesi (`core/s-shutdown-hang`)

`ServerHandle::stop` artık her zaman bitiyor (DESIGN §9.1, BACKLOG §1
satır 4a). Kök neden: `stop()` Shutdown'ı kuyruklayıp ticker'ı hemen
iptal ediyordu; stop anında kontrol kapasitesinden (128) fazla üye
koparsa DETACH'ler oda kanalını dolduruyor, registry satır içi
`send(Shutdown).await`'te sonsuza dek bekliyordu — elindeki `Ticker`
yüzünden broadcast kapanmıyor, oda `Closed`'u, toplayıcı da sonunu hiç
görmüyordu. Aynı sınıftan ikinci bekleme destroy yolundaydı (stop'la
yarışan bir `DestroyRoom` kontrol düzlemini tıkıyordu).

- **Düzeltme:** `registry/actor/stop.rs` — `post_stop`: yer varsa
  `try_send`, kanal doluysa spawn'lu gönderici, oda yoksa hiçbir şey.
  `on_shutdown`/`on_destroy_room` senkron; registry her zaman çıkıp
  `Ticker`'ını düşürüyor, oda `Closed`'dan çıkıp `on_shutdown` +
  `match_result`'u koşuyor. Ticker çalışıyorsa Shutdown sırayla teslim
  edilir. Kural: registry'nin tek await'i kendi posta kutusu. Wire
  baytları değişmedi; B12 (istemciye kapanış bildirimi) bu turda yok.
- **Elenen:** yalnız `try_send` (dolu kanalda at — ticker çalışırken oda
  hiç durmaz), ticker'ı registry bitince iptal etmek (sınırsız bekleme),
  odanın `Closed`'da kanalı boşaltması (kilidi çözmez), registry'nin
  `Ticker`'ı erken düşürmesi (destroy yolunu çözmez), büyük/sınırsız
  kanal.
- **Testler:** `gsb-core/tests/shutdown.rs` (tek oda, shard'lı, destroy;
  ticker önce iptal, 12 üye / kapasite 2), `shutdown/destroy.rs` (ticker
  çalışırken sharded destroy, düz ve dolu posta kutusu),
  `gsb-server/tests/server_stop.rs` (gerçek TCP), `post_stop` birim
  testleri. Düzeltmesiz kodda entegrasyon testleri 5/5 koşuda kilitlendi;
  yeni testler 20× yeşil. Ebeveynin bağımsız mutasyonu (sharded kolda
  hiçbir şey göndermemek) ilk teslimde hiçbir testi kırmıyordu — eskiden
  beri var olan kapsam boşluğu; ajan `destroy.rs`'i ekledi, mutasyon
  artık 2 testi kırıyor. Loadgen 500 (arena, MMO; TCP) her koşu bitti,
  `errors=0`. Testler 713 → 722 (+9).

## H turu — rUDP el sıkışma kaybı (`net/h-udp-handshake`)

İstemci proof'u GÖNDERİNCE kendini bağlı sayıyordu; aynı anda 200+ el
sıkışmada loopback'te sunucu soketinin alım kuyruğu taşıyor, proof
kayboluyor, AUTH boşa düşüp REL bandı 5 sn sonra ölüyordu (U'nun yan
bulgusu, BACKLOG 4b). Artık sunucu doğrulanan proof'a **kabul**
(`ACK{1}`, 5 B) yolluyor; `UdpClient::connect` ancak sunucu oturumu
tuttuğunu gösterince (kabul ya da herhangi bir oturum datagram'ı — o
kaybolmaz, teslim edilir) dönüyor; o zamana kadar güncel adım (challenge
isteği/proof) 50 ms'de bir yeniden gönderiliyor, 5 sn'de (REL canlılık
sınırı) temiz `TimedOut`. Sunucu proof'ta idempotent: kurulu adresten
gelen geçerli proof oturumun güncel ACK'iyle cevaplanıyor — ikinci
oturum/`ConnectionId` yok; challenge isteği ya da geçersiz proof
cevapsız (yansıtma yok). Vazgeçme sınırı < cookie dilimi (derleme
zamanı `assert`): yeniden gönderim ilk cookie'yi kullanır, rotasyonu
aşsa da doğrulanır (DESIGN §6 "El sıkışma kaybı", SECURITY §4.2).

- **Bedel (pinlendi):** olaysız el sıkışma +1 datagram (5 B), `connect`
  +1 RTT (QUIC Retry / DTLS HelloVerifyRequest şekli).
- **Sayaçlar:** istemci `challenge_retries`/`proof_retries`, loadgen
  `hs_retries`, sunucu demux `proofs_reanswered`.
- **Elenen:** `SO_RCVBUF` büyütmek (B4; eşiği taşır, kaldırmaz), sunucu
  tarafında el sıkışma hızlandırma (çekirdek datagram'ı sunucu görmeden
  atıyor), kabulsüz ilk-datagram onayı (sessiz istemci bağlı olduğunu
  öğrenemez), her karede cookie.
- **rustdoc:** `udp/` altındaki 17 uyarı giderildi; küçük paketin
  `pub mod udp` üzerindeki geçici `allow`'u kaldırıldı (ebeveyn). Yan
  temizlik: DESIGN'daki eski "250 ms'de vazgeç" cümlesi, demux'ta kalkmış
  `RETRANSIT_MAX` yorumu, orkestratörün `frag_*` anahtarlarını diğerleri
  kadar katı ayrıştırması.

A/B (1a0fc77'e karşı, `--stagger-ms` YOK, dönüşümlü, yük 6–30):
joined/left demo 200 74,68 → 200,200; demo 500 149,161 → 500,500; arena
200 99,70 → 200,200; arena 500 182,176 → 500,500. `gave_up` 202–702 →
0; istemci `retrans_out` ~9–31 k → ~100–700; connect p99 ~0,5–1,5 sn →
69–156 ms; hata/kapanış 0, 30 Hz. TCP demo 200 aynı. Testler 704 → 713
(+9). Ajanın 16 mutasyonu yakalandı; ebeveynin bağımsız mutasyonu
(tekrar gelen proof'a kabulü yeniden göndermemek) 2 testi kırıyor.

## Küçük paket turu (`misc/small-bundle`)

Altı bağımsız madde, her biri kendi katmanında ve kendi commit'inde.

- **G3-3 / A6 — oturum yükü (kit + arena):** kit'e `Game::session_private`
  kancası (varsayılan: hiçbir şey yazılmaz — altı kit odasında diğer
  oyunların baytları kilitli). `Private.game = 4`'ü oturum başına BİR kez
  doldurur: join ve resume sonrası ilk private kare (resume'da istemci
  yeni bir süreç olabilir); göçte değil (oturum sürüyor). Yük odanın
  zaten gönderdiği karenin sonuna eklenir; yalnız başka bir şey
  gitmiyorsa kendi karesini açar. İstemci: `ClientDecoder::session_private`
  ve `PrivateEvent::Session`. Arena `Welcome { team, teams }` gönderiyor —
  turun tek kasıtlı wire değişikliği, bayt bayt kilitli (takım 1/3 →
  `22 04 08 01 10 03`); arena load botu evini `Welcome`'dan alıyor.
  Elenen: ayrı karşılama opcode'u; JOIN_ROOM_RESULT (çekirdek karesi,
  resume'da yok); yalnız one-shot full (takım odası hiç göndermiyor).
- **G3-2 incelendi, kod değişmedi:** sıra çekirdeğin fan-out'undan
  (önce grup karesi, sonra private). Private'ı öne almak MMO 200'de
  `gap_drops` 147 → 0 veriyor ama aynı delta yine gidiyor, bu kez
  `stale` sayılıyor — yalnız sayaç değişir, üstelik her oyunun kare
  sırası değişir. Gerçek düzeltme (o bağlantıya grup karesini
  göndermemek) wire + çekirdek API değişikliği → BACKLOG.
- **Loadgen CLI:** hatalı komut satırı artık panik değil; stderr'de tek
  satır (`gsb-loadgen: <sebep>`), çıkış kodu 2, `RUST_BACKTRACE`'te bile
  backtrace yok; `--addr`/`--metrics-listen` baştan denetleniyor.
- **rustdoc + CI kapısı:** 106 uyarı satırı temizlendi (bayat yollar,
  özel öğelere bağlar, gereksiz hedefler); CI'da `doc` işi
  (`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`),
  CONTRIBUTING kapı listesinde. `gsb-net/src/udp`'nin uyarıları paralel
  tur sürerken `pub mod udp`'ye geçici `allow` ile bekletildi (H turunda
  kalkar).
- **`max_detach_hold_secs` (sunucu config'i):** saniye (kesir olabilir;
  `0` = uzatma yok, harfiyen), `"off"` = tavan yok, yazılmazsa 10 dk;
  negatif/başka kelime/yanlış tip başlatmayı reddeder. Elenen: "0 = kapalı"
  ve "negatif = kapalı" (yazım hatası sınırsız kilide döner), ayrı boolean
  anahtar. `Config::room_config` üzerinden barındırılan her oyunun
  odalarına (MMO'da her shard'a). Açık: admin `POST /rooms/open` oda
  config'ini varsayılandan kuruyor.
- **Sayaçlar (F3 dahil):** `detach_forced`,
  `effects_{applied,forwarded,orphaned,dropped,refused}`,
  `migrations_{out,in,failed}` — örnek → rapor → gsb-metric satırı →
  Prometheus (`gsb_room_*_total`, OPS §3); loadgen metrik wire'ı GSM9,
  SUM ile katlanıyor. Crystal olayları kit debug satırı olarak kaldı
  (çekirdek örneği sabit şekilli; kit kavramı ya da her örneğe bayt
  ekleyen genel seam gerekirdi — CROSS-SHARD §4c madde 5).

Testler 687 → 704 (rebase sonrası; tur kendi tabanında 670 → 687).
Ajanın her yeni kural için mutasyonu yakalandı; ebeveynin bağımsız
mutasyonu (`"off"`'u 10 dk tavana eşlemek) 2 config testini kırıyor.
Loadgen 200 (arena, MMO): errors=0, server_closes=0, 30 Hz; arena
`gap_drops` 0.

## U turu — rUDP parçalama (`net/u-rudp-fragment`)

Oyun bandında bütçeyi (1472 B) aşan kare artık atılmıyor: yazıcı FRAG
datagram'larına (`4 [u16 id][u8 index][u8 count][parça]`) böler, istemci
birleştirir (DESIGN §6 "MTU", SECURITY §4.1). G3-1 (arena full'ları) ve
C2'nin kümelenmiş MMO yan bulgusu taşımada kapandı. Kit ve çekirdek kodu
değişmedi (yalnız `RoomConfig::max_snapshot_bytes` doc yorumu).
Parçalanmayan her datagram bayt-bayt aynı (pin testi).

- **Ölçüm önce (adım 0):** en büyük tek kayıt 17 B. MMO düellosunun 32 k
  taşmasının 31 k'sı DELTA karesi — kit tarafında "full'u böl" yetmezdi.
  afe7fba'da rUDP'de yazıcı arena 200'de grup datagram'larının %80'ini,
  500'de %93'ünü atıyordu; MMO botları full'ları düşünce kendini hiç
  görmüyordu.
- **Kayıp:** eksik parçalı mesaj düşer (slot'u yeni mesaja geçince ya da
  250 ms), yeniden gönderim yok. **Sınırlar:** mesaj başına 16 parça
  (~23 KB; aşan eski yoldan atılır+sayılır), 4 slot, oturum başına
  64 KiB, datagram başına O(1). Kontrol bandı parçalanmaz (aşan kontrol
  karesi seq harcamadan oturumu bitirir — eskiden akışı tıkıyordu);
  istemci → sunucu FRAG reddedilir (`frag_refused`).
- **Sayaçlar:** yazıcı `frag_messages`/`frag_datagrams`/
  `dropped_oversized`; istemci `frag_reassembled`/
  `frag_dropped_incomplete`/`frag_rejected`; loadgen RESULT
  `frag_reassembled`/`frag_dropped`. rUDP'de `snap_overflows` artık kayıp
  değil bant/parçalama sinyali.
- **Parçalamanın açığa çıkardığı iki istemci hatası düzeltildi:**
  canlılık saati kuyruk boştan doluya geçerken başlıyor; yeniden
  gönderim turu her datagram'dan sonra da koşuyor (olmadan arena 500'de
  `gave_up` 76–106).

A/B (`--transport udp --stagger-ms 5`, dönüşümlü): yazıcı düşürmeleri
arena 200 ~45 k → 0, arena 500 ~100–124 k → 0, demo 200 ~54,8 k → 0,
MMO 500 düello ~10,4 k → 0; `frag_dropped=0`, `joined = left = N`,
0 kapanış, ~30 Hz. Arena 500 rUDP step/bant TCP ile aynı bantta.
Testler 670 → 687 (+17). Ajanın 20 mutasyonu yakalandı; ebeveynin
bağımsız mutasyonu (bellek tavanını 4× gevşetmek) `reassembly_memory_is_
capped`'i kırıyor.

**Yan bulgular (önceden vardı, bu turda düzeltilmedi → BACKLOG §1):**
(1) rUDP el sıkışması: 200+ eşzamanlı bağlantıda loopback'te proof
kayboluyor, istemci kendini bağlı sayıyor (afe7fba'da 200'ün 65–101'i
katılabiliyordu; ölçümler bu yüzden `--stagger-ms 5`); (2) çekirdek
kapanış kilitlenmesi: `stop()` ticker'ı hemen iptal ediyor, registry
dolu oda kontrol kanalında `send().await`'te bekliyor (stop anında 128'den
fazla canlı üye koparsa) — bir MMO ve üç arena-500 koşusu asılı kaldı.

## K4 turu — oyuncu kimliği → ev shard'ı (`kit/k4-player-home`)

Çekirdek, sharded odanın join yönlendiricisine ve oyunun join kancasına
oyuncunun **doğrulanmış kimliğini** veriyor (GAME-MODULE "K4 sonucu"):
`registry::HomeShard = Arc<dyn Fn(ConnectionId, &str) -> usize + Send +
Sync>` (`BuiltRoom::Sharded::home_shard`'ın tipi) ve
`GameLogic::on_join_as(world, conn, identity)` (varsayılanı `on_join`;
oda ve shard taze join'de onu çağırır). Kimlik, çekirdeğin zaten resume
anahtarı olarak taşıdığı dize: ticket'ın `player`'ı, ticket'sız
geliştirme yolunda iddia edilen `Auth.name` (herkes her ad olarak
girebilir — SECURITY §4b), anonimde boş. Yeni tip, yeni mesaj alanı yok;
wire baytları aynı. `PlayerId` taşıyıcı olamaz: join'in ÇIKTISI.

- **Kit:** `Game::spawn_player_as` (varsayılanı `spawn_player`; açık,
  AOI, sektör, sharded ve sharded×spatial odalar çağırır; takım odası
  henüz çağırmıyor — W paketinde).
- **MMO:** `Realm::logins` kimlikle anahtarlı (`with_login(name, pos)`,
  `saved`); yönlendirici ve spawn aynı tabloyu okuyor; kaydısız/anonim
  oyuncu varsayılan durak taşında. Park edilmiş karakter park edildiği
  shard'da resume ediyor (broadcast-resume yönlendiriciden önce).
- **Loadgen:** MMO'yu bot kadrosuyla barındırıyor (`lg-{id}` → waystone
  `id mod 4` halkası); ilk-`Travel` hilesi kaldırıldı, bayrak tutulmadı
  (sayılar G3 bandında).
- **Kasıtlı API değişikliği:** `home_shard` closure'ları iki argüman
  alır (`|conn, _identity: &str|`); `Realm::with_login` sayı yerine ad
  alır.

Testler 664 → 670 (çekirdek `join_identity` 2, kit 1, `mmo_home` 2,
loadgen botu 2, yerine geçen dağılma testi −1). Ajanın mutasyonları
(registry/shard/oda boş kimlik, MMO boş arama, ticket yerine `Auth.name`,
resume broadcast'ını atlamak, loadgen'in katalog MMO'yu barındırması)
yakalandı. Ebeveynin bağımsız mutasyonu (`Realm::saved` hep `None`):
5 test kırılıyor (MMO birimi, loadgen kadrosu, `mmo_e2e`, iki
`mmo_home`).

**Ölçüm** (`gsb-loadgen 200 --game mmo --duration 10 --write-stall-secs
0`, release, yük 5,0 / 4,6 / 3,8): `shard_members` 53,50,50,47 /
54,49,50,47 / 54,47,51,48 — ilk `Travel` olmadan; step p50/p90 120/160,
104/152, 120/160 µs; `out_bps_per_conn` ~21,4 k; `errors=0`,
`server_closes=0`, 30 Hz, `gap_drops=155` (G3 ile aynı).

## D turu — göç tick'i: ölümlü kopyaya yerel darbe (`fix/d-migration-tick`)

C2'nin yan bulgusu kapatıldı (CROSS-SHARD §4d). Göç eden entity eski
shard'ın dünyasında `h + 1`'in migrate fazına dek kalıyor; o tick'te ona
inen YEREL darbe (MMO melee'si, alan etkisi — dünya sorgusu) durumu
zaten giden kopyaya yazılıp kayboluyordu, kopyanın sistemleri ve botu
da `h + 1`'i yeni sahibin yanında ikinci kez oynatıyordu.

- **Kit:** göç tick'inde kopya oyunun kancaları boyunca `Disabled`
  (sorgular görmez) ve `Seam`'de yeni sahibin ödünç kaydı (`h`'de
  yakalanan kayıtla): `local` = `None`, `lent`/`lent_iter` bir kez,
  `emit` yeni sahibe; bot sürmez; sistemlerden sonra kopya kit'e geri
  döner. Darbe yeni sahipte `h + 2`'de, C1'in kimliği/dedup'ı/sırasıyla
  bir kez uygulanır. `ShardGame`/`Seam` imzaları ve MMO kodu değişmedi.
- **Çekirdek (tek, küçük):** `CrossSeam::departed(wire)` (yönlendirme
  tablosunu okur), `emit` kimsenin ödünç vermediği hedefi oraya yollar;
  `SeamStage::depart`. Commit bilgisi yalnız çekirdekte olduğu için
  kaçınılmaz; mesaj/faz/protokol değişmedi.
- **Sınır:** saklanmış bir `Entity` tutamağıyla doğrudan yazma
  `Disabled`'ı atlar; hedef wire'dan çözülmeli (HANDOFF'ta not).
- Client wire baytları aynı.

Testler 657 → 664 (çekirdek 1, kit 4, MMO gerçek aktörlerle 2: bölge
geçişi düellosu ve kristal bırakma — hasar toplamı = kayıp can, kill
kredisi doğru). Ajanın 16 kit + 2 çekirdek mutasyonu yakalandı.
Ebeveynin bağımsız mutasyonları: kopyayı gizlememek → 3 kit + 2 MMO
testi, çekirdekte `departed` yedeğini kaldırmak → 1 çekirdek + 2 MMO
testi kırılıyor. Loadgen sağlaması (MMO 200, varsayılan ve düello):
errors=0, server_closes=0, 30 Hz.

## Cross-seam C2 turu — crystallization, histerezisli (`xseam/c2-crystallize`)

CROSS-SHARD §4 katman 4 uygulandı (tasarım, elenen alternatifler, beş
gerekçeli sapma: CROSS-SHARD §4c). `gsb-core` DEĞİŞMEDİ; client wire
baytları aynı. **Cross-seam etkileşim paketi bitti.**

- **Tespit kit'te:** opt-in oda (`with_crystallize(Crystallize)`) seam
  ötesi kontakları — giden `Seam::emit`, uygulanan uzak etki, oyunun
  `Seam::contact` ile bildirdiği yerel darbe — sırasız wire çifti başına
  sınırlı bir dövüş tablosunda tutar (1024 çift, `window` sessizliğinde
  düşer). Seri K tick'e yayılmış ve iki yön canlıysa çiftin YÜKSEK wire'ı
  alçak wire'ı ödünç veren shard'a mevcut göçle (`KitMig.pin`) taşınır —
  mesajsız, kilitsiz.
- **Sahiplik bölgeden ayrışır** (tasarımın kilit kararı): alıcı shard
  mover'ı ve partnerini pinler; `collect_migrations` önce pin'e bakar,
  yoksa `region_of`'a — pin olmasa mover eski bölgesine hemen geri
  verilirdi (ping-pong). Bırakma: `release` tick sessizlik, bant dışı
  (`Partition::holds`) ya da partnerin gitmesi; giriş yarım bantla
  (uzamsal histerezis). Pinli entity mover olmaz; köşe dövüşü en düşük
  wire'ın shard'ında toplanır.
- **MMO:** `world::CRYSTALLIZE` (K 1 sn, pencere 2 sn, bırakma 3 sn, bant
  64 m); yerel darbeler `Seam::contact` ile bildiriliyor; sunucu
  `[mmo] crystallize = true|false` (vars. açık). Devir tick'indeki darbe
  C1'in yönlendirmesiyle bir kez uygulanıyor.
- **Loadgen:** `--mmo-duel-frac F`, `--mmo-crystallize on|off`
  (varsayılan bot girdi-girdi aynı).
- **Ölçüm (90 sn, F = 0,2, on/off, 200 ve 500 istemci):** maliyet gürültü
  içinde (30 Hz, adım p50/p90 ve bant aynı); uzak etki 200'de ~%25, 500'de
  ~%9 azaldı; 0,27–0,40 crystal göçü/sn; hiçbir wire 90 sn'de iki
  kereden fazla taşınmadı. Varsayılan yükte A/B gürültü içinde.

Testler 641 → 657; 25 kit + 3 MMO/çekirdek mutasyonu yakalandı.
Ebeveynin bağımsız mutasyonu (pin'i yok saymak): kit'te 5, MMO'da 1 test
kırılıyor. **Yan bulgular:** kümelenmiş MMO yükünde (500, düellocular)
`snap_overflows` ~32 k — crystallization'dan bağımsız, snapshot bölme
ihtiyacı; göç eden entity eski dünyasında bir tick daha kalıyor ve o
tick'teki YEREL darbe o kopyaya iniyor (her göçte var, yeni değil).
Arena ve MMO crate belgelerindeki "sunucuya bağlı değil" cümleleri
güncellendi (G2'den beri bayattı).

## Cross-seam C1 turu — oynanış seam'in ötesini görüyor ve etkiliyor (`xseam/c1-remote-effect`)

CROSS-SHARD §2–§4'ün uzak-etki kısmı uygulandı (tasarım + sekiz gerekçeli
sapma: CROSS-SHARD §4b). `gsb-core` mesaj biçimli, minimal değişti;
client wire baytları aynı.

- **Ödünç kayıtlar oynanışa açık:** sharded tick kancaları
  (`ShardLogic::ingest_seam`/`update_seam`, varsayılanlı) bir
  `CrossSeam` alır — aktörün komşu görünümleri yerinde okunur (kopya
  yok, karantina hariç, bayatlık ≤ 1 tick). Kit'in `Seam`'i sahip-olunan
  wire'ları ayıklar (own wins); `ShardGame::{ingest_seam, systems_seam,
  apply_remote_effect}` varsayılanlı — hiçbir mevcut oyun değişmedi.
- **`ShardMsg::RemoteEffect`:** ödünç veren komşuya yönlenir; kaynağın
  tick'i + 1'de, `(source, origin, seq)` sırasıyla uygulanır; köken
  başına sabit kayan pencere ile idempotent (1024 seq; sınır bütçe ×
  yaş tavanından türetildi); göç etmiş hedef eski sahipten yeni sahibe
  iletilir (TTL 11 tick, ≤ 3 atlama); dolu link'te sınırlı yeniden
  deneme (sonraki tick, tavan 1024, yaş 7 tick). Epoch = registry kurulum
  nesli.
- **MMO:** `Attack` seam ötesine uzanıyor — saldıranın shard'ında menzil,
  sahibinde politika (bayatlık 3 tick, menzil yeniden denetimi, hasar
  tavanı); oyuncular da hedef (0 hp = en yakın waystone'da yenilgi);
  vurulan oyuncuyu kendi shard'ı savaşta işaretler (çıkış vetosu seam
  ötesi dövüşü görür); kill kredisi otoritenin savaş akışında. Aynı
  tick'te iki taraftan gelen darbede sonuç, istemci sırasından bağımsız
  olarak aynı.
- **A/B (200 istemci, 3+ çift, `--write-stall-secs 0`):** demo sharded,
  spatial × sharded ve MMO gürültü içinde (loadgen MMO botu seam'lerden
  uzakta dolaştığı için ölçülen yalnız boştaki maliyet).

Testler 623 → 641 (çekirdek 10, kit 3, MMO 5); 21 mutasyonun hepsi
yakalandı. Ebeveynin bağımsız mutasyonu (tekrar kontrolünü kapatmak):
çekirdekte 2, MMO'da 1 test kırılıyor.

## Oyun modülü G3 turu — loadgen her barındırılan oyunu sürüyor (`srv/g3-loadbot`)

`gsb-loadgen --game demo|arena|mmo` (varsayılan `demo`): oyun başına bir
`LoadBot` (dyn) girdiyi, flood/churn girdisini ve snapshot/private
opcode'larını verir; istemci başına `BotClient` kit'in `ClientView<D>`'sini
oyunun çözücüsüyle tutar (loadgen-yerel görünüm yok — G4). `--game`
süreç içi sunucuya, `--serve`'e ve orkestratörün sunucu + istemci
çocuklarına iletilir; bilinmeyen oyun derlenmiş oyunları listeler, başka
oyun için yazılmış demo bayrağı (`--visibility`, `--topology`,
`--shard-count`, `--cell-size`, `--vision-radius`, `--spawn-half-size`,
`--disconnect-grace-secs`, `--profile`, `--still-frac`) hata verir.

- **Demo botu birebir taşındı:** 576 000 aralıkta girdiler bayt-bayt aynı;
  A/B'de RESULT/CLIENT anahtarları, sırası, biçimi aynı, sayılar gürültü
  içinde, alıcı döngü yavaşlamadı.
- **Arena botu:** spawn (takım üssü) → merkez → spawn, 8 sn, 0–20 m
  yükseklik — takımlar birbirinin 3D sisine gerçekten girip çıkıyor.
- **MMO botu:** ilk girdi waystone `id mod 4`'e `Travel` (K4: yoksa her
  oturum shard 0'da kalır), sonra waystone çevresinde dolaşma, ~20 sn'de
  bir `Travel`, menzilde ~1 sn'de bir `Attack`; rastgelelik istemci
  kimliğiyle tohumlanıyor.
- **RESULT:** arena/MMO'da `visibility`/`shards`/`profile` oyunun düzeni;
  MMO'ya `game=`'den önce `shard_members=`; `game=` son anahtar.

**İlk tabanlar** (`--write-stall-secs 0`, 10 sn; 1000'ler `--orchestrate
--procs 2`; hepsinde 30 Hz, bütçe aşımı %0, `errors=0 server_closes=0`):

| Oyun | N | step p50/p90 µs | bayt/sn/bağlantı | notlar |
|---|---|---|---|---|
| arena | 50 | 104/144 | 10 669 | |
| arena | 500 | 1104/1640 | 111 508 | tepe payload 5162 B |
| arena | 1000 | 1632/2024 | 234 120 | tepe payload 10 267 B |
| mmo | 50 | 48/80 | 5 465 | shard'lar 14,11,13,12 |
| mmo | 500 | 224/296 | 49 096 | 125,111,136,128 |
| mmo | 1000 | 288/400 | 102 190 | 245,245,255,255 |

Tam tablo ve komutlar: GAME-MODULE §5 "G3 sonucu". Ebeveynin bağımsız
koşusu (200 istemci, 6 sn, üç oyun): üçü de `joined=left=200 errors=0
server_closes=0`, 30 Hz; MMO `shard_members=55,50,49,46`.

**Bulgular:** G3-1 — arena'nın full snapshot'ları ~150 birimden sonra
rUDP'nin 1400 B sınırını aşıyor (TCP'de zararsız; rUDP için delta ya da
bölme gerekir). G3-2 — join'de grup delta'ları tek seferlik private
full'dan önce gelebiliyor (≥200 istemcide istemci başına ~0,8
`gap_drops`, kayıp yok; demo'da da vardı, görünüm doğru işliyor). G3-3 —
arena istemcisi takımını wire'dan öğrenemiyor (bot spawn konumundan
çıkarıyor; düzeltme korunan crate'te). Küçük kusur: loadgen komut satırı
hatalarını panikle bildiriyor (G3 öncesinden gelen alışkanlık; mesajlar
doğru, çıkış kodu sıfır değil).

Testler 609 → 623.

## Oyun modülü G4 turu: kit'in referans istemcisi (`srv/g4-client`)

Kit zarfının istemci kuralları tek bir kit modülünde: `gsb_kit::client`
(`docs/GAME-MODULE.md` §5 "G4 sonucu", `docs/KIT-ARCHITECTURE.md` §5.2).
G3'ten ÖNCE koşuldu (ebeveyn kararı: kuralları önce loadgen'e, sonra
kit'e yazmak aynı kodu iki kez yazmak olurdu; G3'ün botları bu görünümü
kullanacak). `gsb-core` değişmedi; wire baytları aynı.

- **`ClientView<D: ClientDecoder>`** (`513a419`): grup ve `Private`
  karelerini ham bayt olarak alır, `kit.proto`'nun istemci kurallarını
  uygular (full değiştirir; baseline'lı delta removed → cell_exits →
  upsert; baseline'sız delta düşer; son KABUL edilmişten `<=` sequence
  atılır — ilk kabulden önce hiçbir şey bayat değil; one-shot private
  full koşulsuz; private delta hata) ve loadgen'in sayaçlarını verir.
  Oyunun seam'i: kayıt → `(id, Record)`, `cell_of(&Record)`, çıkış →
  hücre. Zarf ayırmasız iki geçişte yürünür; yürüyücü public
  (`client::wire`).
- **Altı kopya kit'e geçti** (`2809a87`, `e652e82`, `b4c0039`): loadgen
  görünümü, `delta_aoi.rs`, `aoi.rs`, MMO'nun iki test istemcisi, örnek
  istemci; her iddianın koşulu ve mesajı aynı.
- **Hata düzeltmesi (bulgu G4-1, `d0c88d2`):** loadgen ve `delta_aoi`
  `CellExit`'i konum sanıp hücreye çeviriyordu (`CellExit(1,0)` →
  hücre (0,0)); indisi kendine eşlenmeyen her çıkışta yanlış hücre
  unutuluyor, çıkan hücre keep-alive'a dek hayalet kalıyordu. Sayaçlar
  etkilenmiyordu; tutulan görünüm karelerin %3–9'unda farklıydı. Yeni
  test önce kırıldı.
- **Alıcı döngü daha hızlı** (`3a77e9b`, `4667c26`, `cfd7d3e`): aynı
  karede kit görünümü eski tipli çözümün 0,63–0,79'u (tek kayıtlı
  karelerde başabaş).

Test sayısı 586 → **609**. Loadgen A/B (üç senaryo, dönüşümlü üç tur):
CLIENT/RESULT anahtarları, sırası ve biçimi aynı, sayaçlar gürültü
içinde. Ebeveynin bağımsız mutasyonu (bayat kuralı `<=` → `<`) iki
kural testini kırıyor.

## Kit düzeltme turu: K1–K3, K5, tek-oyun CI (`fix/kit-input-migration`)

G2'nin gerçek sunucu altında bulduğu kit bulguları kapandı
(`docs/GAME-MODULE.md` §5 "Kit düzeltme turu"). `gsb-core` değişmedi;
wire baytları aynı.

- **K1–K3 — oyuncunun girdi oturumu göçle taşınıyor** (`4d83d01`):
  `KitMig`'e `input: Option<ShardInputRecord { hwm, acked }>`;
  `collect_migrations` okur (reddedilen gönderimde kaynakta kalır),
  `on_migrate_out` siler (K3: kaynakta sızıntı yok), `on_migrate_in`
  kurar (`InputSeq::adopt`). Göçü tetikleyen girdi (MMO'nun `Travel`'ı)
  artık hedef shard tarafından tek kez ack'leniyor (K1); sıra kuralı
  göçte sıfırlanmıyor, eski/çift datagram hedefte düşüyor (K2). İki
  sharded odada da; 2D demo'nun sharded odaları aynı yolda. `KitMig` ve
  kayıtları `sharded/mig.rs`'e taşındı. Göç mesajı 24 bayt büyüdü (demo
  `ShardMsg` 112 → 136, MMO 144 → 168); gelecekteki Ipc/Net codec'i yeni
  alanı kapsamalı. `mmo_findings` kilitleri doğru davranışa çevrildi.
  Ebeveynin bağımsız mutasyonu (varışta taşınan kaydı yok saymak): 7 kit
  testinin 6'sı ve iki gerçek-sunucu MMO testi kırılıyor.
- **K5 — örnek config oyun değiştirmeye hazır** (`80da518`):
  `config.example.toml` demo'nun beş düz anahtarını varsayılanlarıyla
  YORUMDA yazıyor; kopyada yalnız `game` satırını değiştirmek arena /
  MMO'yu başlatıyor. Değiştirilmemiş örnek demo'yu aynen eskisi gibi
  barındırıyor (aynı çözülmüş seçim).
- **CI** (`c8fa79a`): `no-game` işi her oyun özelliğini tek başına da
  derliyor ve lint'liyor.

Test sayısı 579 → **586** (+7 kit birim testi). Loadgen A/B (50 istemci,
4 shard, sharded ve sharded × spatial, dönüşümlü üçer çift) gürültü
içinde; istemcinin saydığı ack'ler 2291–2296 → 2297–2298 (göçte
kaybolanlar geri geldi).

## Oyun modülü G2 turu (`srv/g2-modules`)

Arena ve MMO artık gerçek sunucuda, gerçek istemcilerle uçtan uca
çalışıyor (`docs/GAME-MODULE.md` §5 "G2 sonucu"). `gsb-core`, `gsb-kit`
ve üç demo crate'i değişmedi.

- **Arena modülü** (`games/arena.rs`, `game = "arena"`, varsayılan açık
  `game-arena` özelliği): tek oda × takım sisi × always-full
  (`TeamRoom<ArenaGame, VisionGrid3<Pos3>>`). `[arena]`: `teams`
  (1..=255, vars. 3), `disconnect_grace_secs` (vars. 30; sonunda arenanın
  üsse dönüş botu).
- **MMO modülü** (`games/mmo.rs`, `game = "mmo"`, `game-mmo`): her oda
  kimliği bütün bir 2×2 shard'lı dünya. Join yönlendirmesi: kayıtlı
  karakter → kaydının shard'ı; kaydı olmayan → waystone 0'ın shard'ı
  (`spawn_player`'ın geri düşüşüyle aynı yer). `[mmo]`:
  `logout_grace_secs` (vars. 20, savaş vetosu korunur), `logout =
  "release" | "bot"`. `MmoModule::with_realm` gömen taraf için.
- **Reddedilenler:** iki oyun da üç ekseni, `shard_count`,
  `aoi_cell_size`, `team_vision_radius`, `spawn_half_size` ve demo'nun
  düz `disconnect_grace_secs`'ini AÇIKÇA yazılırsa başlatmada reddeder
  (MMO'nun mesajı `[mmo] logout_grace_secs`'i gösterir); oyunun
  tablosunda bilinmeyen anahtar da hata. Okuyucular public
  (`games::settings`). Bilinmeyen oyun hatası üç oyunu listeliyor.
- **Uçtan uca testler** (`tests/hosted/` ortak istemci, TCP + TLS):
  arenada her snapshot'ta ağ tarafından sis kuralı + yükseklik + ack;
  MMO'da yönlendirme (basan shard wire id'den), dikiş geçişi, `Travel`,
  savaşta tutulan / grace sonrası çıkan karakter + registry satırlarının
  bırakılması, başka shard'da park edilmiş karakterin resume'u,
  `room_count = 2` + `/rooms/open` ile üç ayrı dünya; config dosyasından
  tablolar ve retler; eski demo config'i aynen.
- **Bulgular (kit):** oyuncunun girdi durumu (`InputSeq`) göçte
  taşınmıyor — göçü tetikleyen girdi hiç ack'lenmiyor (K1), sıra kuralı
  hedefte sıfırlanıyor (K2), kaynakta girdi girdisi sızıyor (K3).
  Bugünkü davranış `tests/mmo_findings.rs`'te kilitli; düzeltme kit'te.
  K4: kayıtlı karakterler oturuma bağlı — gerçek sunucuda her MMO
  oturumu shard 0'dan başlar. K5: `config.example.toml` demo'nun düz
  anahtarlarını açıkça yazdığı için kopyası başka oyuna çevrilince
  reddediliyor.

Testler 551 → 579 (+10 birim, +18 entegrasyon); mevcut testlerin hiçbiri
değişmedi. Ebeveynin bağımsız mutasyonu: MMO'nun sabit-anahtar
listesinden `visibility`'yi çıkarmak birim testini kırıyor. Loadgen
(demo) `left=50 errors=0 … game=demo`.

## Oyun modülü G1 turu (`srv/g1-module`)

Sunucu artık barındırdığı oyunu tek bir nesne-güvenli seam'in arkasında
tutuyor (`docs/GAME-MODULE.md` §4.1, §5 "G1 sonucu"). `gsb-core`,
`gsb-kit` ve üç demo crate'i değişmedi.

- **`GameModule` + `RegistryParts`** (`gsb-server/src/game.rs`): trait'in
  generic parametresi yok; `RegistryParts::spawn<W, G, St, Sp>` tek generic
  metot ve registry'nin sınırlarını birebir taşıyor. `spawn_registry`
  yalnız `RegistryParts::spawn`'ın üretebildiği bir `RegistryTask`
  döndürür. `start_inner`'ın altı kollu `match`'i
  `module.spawn_registry(parts)`'a indi; mesaj tablosu = base +
  `module.register`.
- **2D demo modülü** (`src/games/demo/`): fabrikalar (`git mv`),
  üç-eksen çözümleyici, `RoomKind`/`VisibilityAxis`/`ResolvedSelection`,
  ekonomi servisi, shard-sayısı kontrolü olduğu gibi taşındı. Uyumluluk:
  `Config`'in demo alanları, `Config::resolve_selection`, kökteki
  `build_table` ve seçim tipleri yerinde; eksen hataları `ServerError`'da,
  mesajları bayt-bayt aynı.
- **Oyun seçimi:** `game = "demo"` (varsayılan); bilinmeyen ad başlatmada
  derlenmiş oyunların listesiyle reddedilir (`ServerError::Game`).
  `Config::raw` ayrıştırılmış dosyayı modüle taşır. Açık modül için
  `start_game_server(module, cfg)` / `start_game_server_with(…)`.
- **Oyunsuz derleme:** `gsb-demo` isteğe bağlı, varsayılan açık
  `game-demo` özelliği; `cargo build -p gsb-server --lib
  --no-default-features` oyunsuz derleniyor — CI `no-game` işi.
  `gsb-loadgen` ve örnek istemci `required-features = ["game-demo"]`.
- **RESULT:** satırın sonuna `game=<ad>` (tek değişiklik).
- **Hata düzeltmesi (karar 10):** orkestratör istemci çocuklarına
  `--cell-size` iletmiyordu; istemci görünümü 20 ile hücre hesaplarken
  sunucu iletilen değeri kullanıyordu. Önce kırılan iki testle; ebeveynin
  bağımsız mutasyonu (bayrak adını bozmak) ikisini de kırıyor.
- **Kasıtlı davranış değişikliği (karar 11):** ekonomi servisi artık altı
  demo odasının HEPSİNE bağlı — AOI, team ve PVS odaları `ECONOMY`'ye
  "economy service not configured" yerine gerçek cevap veriyor.
  `tests/economy_rooms.rs` altı yapının her birinde bir gidiş-dönüş
  sürüyor (düzeltmeden önce üçü kırıktı).

Testler 537 → 551 (+2 orkestratör, +6 ekonomi, +5 modül, +1 varsayılan
kilidi); mevcut iddialar değişmedi (`loadgen_smoke`'a `game=demo`
iddiası eklendi). Loadgen A/B (dönüşümlü; tcp, spatial, team,
sharded×4): her anahtar aynı sırada ve biçimde, sayılar gürültü içinde,
sonda `game=demo`.

**Elenen alternatifler:** `start_server_with(module, cfg)` adı (mevcut
public `start_server_with(cfg, hooks)` ile çakışır → `start_game_server*`);
eksen hatalarını `GameError`'a taşımak (mevcut testler ve çağıranlar
kırılırdı; yalnız üretici taşındı); `spawn_registry → JoinHandle<()>`
(bir modül registry'yi başlatmadan herhangi bir görev döndürebilirdi →
`RegistryTask`); ham tabloyu ayrı parametreyle taşımak (`start_server(cfg)`
yalnız `Config` alıyor → `Config::raw`); `Config` varsayılanları için
`gsb-kit`'e koşulsuz bağımlılık (oyunsuz derlemeyi kit'e bağlardı →
sunucu-yerel sabitler + kilit testi); ekonomi düzeltmesi için `gsb-demo`'ya
`with_economy` eklemek (korunan crate → kit'in public `game_mut()`'u).

**G2 için karar:** `game = "…"` dizesi ile `[game]` tablosu aynı TOML
belgesinde birlikte olamaz → oyunların tablosu kendi adını taşır
(`[arena]`, `[mmo]`; GAME-MODULE §6 karar 1).

## Küçük düzeltme paketi: sharded × spatial çerçeve süzgeci + dolu mailbox'ta stall hükmü (`fix/small-bundle`)

**Commit'ler** (`08e6e13..`): `3b5b7ca` kit (çerçeve süzgeci), `ff9b8ea`
gsb-net (ayrılmış hüküm slotu). `gsb-core` ve `gsb-server` dokunulmadı;
wire baytları ve demo iddiaları aynı.

### 1. `ShardedSpatialRoom` artık `Partition::admits`'i uyguluyor (`3b5b7ca`)

Faz 5'in yan gözlemi (KIT-ARCHITECTURE §10): düz `ShardedRoom` ödünç
şeridi `admits` ile süzüyor, kompozit bütün şeridi hücre defterine
alıyordu. "Görünürlük hücreyle sınırlı, yani doğru" varsayımı genelde
tutmuyor: hücre kenarı bölgeye göre büyükse bir grubun 3×3'ü komşunun
UZAK kenarına uzanıyor. Kit fikstüründe (4×4, bölge 25, kenar payı 6,25,
hücre 20) shard 0'ın `Cell(-2,-2)` grubu shard 1'in doğu kenarındaki
(x = -1) kaydı görüyordu; düz oda aynı kaydı reddediyor.

- **Düzeltme:** `integrate_borrowed` şeridi deftere almadan önce aynı
  süzgeçten geçiriyor; defter SÜZÜLMÜŞ görünümü tutuyor (çerçeveden çıkan
  kayıt çıkış, geri giren giriş). F1'in çıkış-atlama kuralı değişmedi.
- **Kanıt:** `sharded/tests/frame_filter.rs` (2), düzeltmesiz ikisi de
  kırıldı. Ebeveynin bağımsız mutasyonu (süzgeci kapatmak) ikisini de
  kırıyor.
- **Etkisi:** loadgen sharded × spatial (N=4, 8 sn) `out_bps_per_conn`
  ~%10 düştü (3229–3278 → 2877–2933, üç çift dönüşümlü) — artık
  gönderilmeyen uzak şerit kayıtları; `snap_total` gürültü içinde.

**Elenen alternatifler.** Süzgeci paket aşamasında uygulamak (defterde
olup pakette olmayan kayıt istemcinin çıkış/giriş muhasebesini bozar);
gözlemi "zararsız" diye açık bırakmak (partition'ın reddettiği kayıt
istemciye gidiyordu — doğruluk kusuru).

### 2. Aşırı yükte `outbound_dead` yanlış atfı (`ff9b8ea`)

"Ölçüm kaydı"nın açık küçük kusuru (10k: koşu başına 67–172
`outbound_dead`). Mekanizma koddan doğrulandı: mailbox hüküm anında
DOLU; pump'ın `try_send`'i düşüyor, hüküm kapanıştan sonraki awaited
`send`'e erteleniyor; uyanan aktör `adopt_pending_close`'da hükmü
bulamıyor ve `outbound_dead` yazıyor.

- **Düzeltme:** writer pump doğumda (mailbox boşken) mailbox'ın kendi
  kapasitesinden bir slot ayırıyor (`try_reserve_owned`,
  `pump/writer/verdict.rs`); stall hükmü giden kanal kapanmadan ÖNCE
  `OwnedPermit::send` ile o slota gidiyor — senkron, dolu mailbox'ta da
  başarısız olamaz. Aktör kodu, imzalar değişmedi; await yok, sınırsız
  kanal yok. Bedel: bağlantı başına bir mailbox slotu.
- **Kanıt:** `gsb-net/src/pump/tests.rs` (gerçek `ConnectionActor` +
  `spawn_pumps`, dolu mailbox); düzeltmesiz her koşuda `OutboundDead`.
  10k ölçümü yeniden alınmadı.

**Elenen alternatifler.** `Arc<OnceLock<ServerClose>>` / atomik sebep
kodu (her kapıya ve `gsb-server`'a yeni parametre; ikinci doğruluk
kaynağı); kanal-kapandı sinyalinin sebep taşıması (`Mailbox` takma adını
sarmalamak çekirdeğe yayılır); kapanıştan önce awaited `send`
(kilitlenme); `adopt_pending_close`'da bekleme (aktöre await); sınırsız
kanal / büyük mailbox (kural; yalnız eşiği kaydırır).

**Doğrulama:** 537 / 0 / 1 ignored; clippy 0; kapanış kontrolü 85.

## Süreli bekletmede `may_release` vetosu + veto tavanı (`fix/may-release-deadline`)

`docs/RECONNECT.md` §17: "kopan karakter 20 sn sonra çıkış yapsın — ama
savaştayken değil" artık ifade edilebiliyor. Önce `GameLogic::may_release`
yalnız süresiz bekletmede (`grace = None`) soruluyordu; süreli bekletme
deadline'ında koşulsuz bitiyordu (KIT-ARCHITECTURE §10, F4 gözlemi).

**Commit'ler** (`e96fc94..`): `32e64f0` çekirdek (oda + shard aktörü),
`00d2e4f` kit belgeleri, `207c390` MMO "çıkış sayacı + savaşta çıkış yok".

**Davranış değişiklikleri (açıkça):**
- **Süreli bekletme deadline'ında veto soruluyor.** `true` → bugünkü gibi
  `ExpireTo`'ya biter (aynı süpürme, aynı sayaçlar); `false` → bekletme
  uzar, soru her süpürmede (her tick'in 0c fazı) tekrarlanır. Hiç veto
  etmeyen oyun için davranış bire bir aynı (kilitli).
- **Yeni `RoomConfig::max_detach_hold: Option<Duration>`** — varsayılan
  `Some(10 dk)` (`DEFAULT_MAX_DETACH_HOLD`), DETACH anından ölçülür;
  tavanda hâlâ duran veto ezilir, bekletme `ExpireTo`'suna biter, aktör
  bir kez uyarır (`detach_ceiling_warns`). Tavan yalnız vetoyu ezer,
  grace'i asla kısaltmaz. `None` = tavan yok; `Some(ZERO)` = uzatma yok
  (literal — `max_idle_input_secs`'in "0 = kapalı"sından bilinçli sapma);
  temsil edilemeyen tavan = tavan yok. Tavan göçle taşınır
  (`PlayerMigration.detach_ceiling`, public struct'a yeni alan).
- **Süresiz bekletmeye de aynı tavan uygulanır** (davranış değişikliği:
  yalnız vetosu 10 dk'dan uzun duran süresiz bekletme farklılaşır;
  `max_detach_hold: None` eski davranışı birebir geri verir).
- **Kit:** kod değişmedi (iletim zaten altı odadaydı), belgeler yeni
  anlama göre güncellendi.
- **MMO:** isabet eden saldırı saldırgana `InCombat { until }` yazar
  (`COMBAT_TICKS` = 180 tick, 6 sn); `MmoGame::may_release` bu işaret
  dururken çıkışı veto eder; işaret göçer (`MmoMig::Player.combat`).

**Kullanıcı kararı ve bir düzeltme:** semantiği kullanıcı onayladı
("süre dolunca veto sorulsun, uzatsın, mutlak tavan geçerli olsun").
Ebeveynin önerisi tavanın zaten var olduğunu (`max_detach_hold`)
söylüyordu — yanlıştı; eski tasarımda tavan grace'in kendisiydi
(RECONNECT §276). Tavan bu turda yeni bir ayar olarak eklendi.

**Elenen alternatifler:** sınırlı geri çekilmeli yeniden sorma (vetonun
kalkışını gecikmeli görür, göçmesi gereken ek durum ister); tavanı yalnız
süreli bekletmeye uygulamak (en açık yol — saf veto — korumasız kalırdı);
tavan = grace'in katı (süresizde tanımsız); grace'i de kesen mutlak üst
sınır (veto etmeyen oyunun davranışını değiştirirdi); oyun başına tavan
(`Detach` API'sine kırıcı ekleme — tavan operatör vanasıdır); `RoomSample`'a
`detach_forced` sayacı (metrik şeması ekleri kapsam dışı, açık iş).

**Testler (+13 → 534):** `gsb-core/src/room/tests/hold.rs` (6),
`gsb-core/src/shard/tests/hold.rs` (5 — göçte tavanın taşınması dahil),
`gsb-demo-mmo/tests/combat_logout.rs` (2, gerçek dört shard aktörü).
Önce kırıldılar (eski süpürmede oda 4/6, shard 4/5; MMO vetosu olmadan
2/2); 13 mutation'ın hepsi yakalandı. Ebeveynin bağımsız mutation'ı:
tavan kontrolünü kapatmak oda + shard'da 6 tavan testini kırıyor.
**Bilerek değiştirilen fikstür:** `gsb-core/tests/reconnect.rs`'in
`ParkLogic.release_ok` varsayılanı `false` → `true` (trait'in "veto yok"
varsayılanı; eski `false` artık her süreli park'ı veto ederdi). Hiçbir
iddia değişmedi.

**Doğrulama:** 534 / 0 hata / 1 ignored; clippy `-D warnings` temiz;
kapanış kontrolü 85/85; loadgen 50 istemci 3 sn `left=50 errors=0`;
churn 50 istemci (`--churn-secs 1.5`) `errors=0 resumed=50
resume_rejected_stale=0` (churn istemcisi LEAVE göndermez: `left=0`
yapısal).

## WS uyum kapısı turu (RFC 6455 denetimi + Autobahn CI kapısı)

HANDOFF iş sırası madde 3. El yazımı RFC 6455 kapısı `[[listeners]]`
üzerinden servis yolunda olduğu için okuyucu RFC 6455 §5 (çerçeveleme) ve
§7'ye (kapanış) karşı kural kural denetlendi. Bilinen açık ve denetimin
bulduğu üç sapma kapandı; her biri önce kırılan testiyle geldi ve
mutation-check'li. Tam kural tablosu (önce → şimdi → test):
`docs/SECURITY.md` §3.7.

**Commit'ler** (`2c8725c..`): `ba3be1e` parça arası veri çerçevesi,
`2c01207` uzunluk kodlaması, `54af477` kapanış çerçevesi doğrulaması,
`564edec` opak eşleme + Autobahn harness'i, `e7cf7bf` CI işi, ardından
bu doküman commit'i.

**Davranış değişiklikleri (açıkça; hepsi yalnız kural dışı istemciyi
etkiler, uyumlu istemci için tel aynı):**
- **Bilinen açık, §5.4:** açık parçalı bir mesajın içinde gelen yeni BIN
  çerçevesi, FIN'siz ise yarım mesajı **sessizce atıyordu**, FIN'li ise
  mesajın **içinde** oyun karesi olarak teslim ediliyordu. Şimdi ikisi
  de 1002. Aynı yerdeki TEXT 1003 yerine 1002 alıyor (ilk kusur
  çerçeveleme). Tek başına TEXT hâlâ 1003: sözleşme kararı değişmedi.
  Düzeltme, iki veri kolunun önünde tek bir guard.
- **§5.2:** MSB'si 1 olan 64-bit uzunluk 1009 (tavan aşımı) sanılıyordu;
  artık tavandan önce 1002. Minimal olmayan uzunluk kodlaması kabul
  ediliyordu; artık 1002.
- **§7.4 / §8.1:** her iki baytlık kapanış kodu yankılanıyordu; artık
  gönderilemez kodlar (0-999, 1004-1006, 1015, 1016-2999, ≥ 5000) 1002,
  UTF-8 olmayan sebep 1007 alıyor. 1000-1003, 1007-1014 ve 3000-4999
  eskisi gibi yankılanıyor (yalnız kod).
- **Yalnız ekleme:** `WsTransport.mapping: WsMessageMapping`
  (`GameEnvelope` varsayılan = tel sözleşmesi; `Opaque` yalnız
  conformance harness'i için, config'ten seçilemez). `WsTransport`'u
  struct literal'iyle kuran dış kod bu alanı eklemeli (`..Default::default()`
  de olur).

**Testler (+24 → 521):** `gsb-net` `ws::tests::fragmentation` (7: üç
açık + dört kilit, parça arası PING/PONG/CLOSE dahil), `ws::tests::framing`
(8), `ws::tests::close_frames` (6), `ws::tests::opaque` (3). Hepsi yeni
okuyucu-seviyesi `ws::tests::rig` üzerinde: çıplak `WsReader` + loopback
soket; kapanış kodu doğrudan giden kuyruktan okunuyor. Önce-kırılan
kanıt: fix'lerden önce 7 test kırmızıydı (üç araya girme; MSB → 1009;
minimal olmayan uzunluk teslim edildi; gönderilemez kod ve UTF-8 olmayan
sebep yankılandı). Mutation'lar (her biri yedekten geri yüklendi):
guard'ı yalnız BIN'e daraltmak → text testi kırılıyor; guard'ı kapatmak →
3 test; MSB denetimini kapatmak, 64-bit minimal sınırını `< u16::MAX` ya
da `< 126` yapmak, 16-bit denetimini kapatmak ya da `<= 126` yapmak →
her biri kendi testini kırıyor; kod denetimini kapatmak, 1015'i
izinlilere katmak, 1012-1014'ü çıkarmak, UTF-8 denetimini kapatmak →
her biri kendi testini kırıyor; opak eşlemenin okuyucu ya da yazıcı
kolunu kapatmak → `opaque` testleri kırılıyor.

**Autobahn kapısı:** CI'da yeni `autobahn` işi. `examples/ws_autobahn`
(release) 127.0.0.1:9001'de başlıyor; `crossbario/autobahn-testsuite:25.10.1`
fuzzingclient modunda ona karşı koşuyor; `.github/autobahn/autobahn.py`
spec'i üretiyor ve raporu yargılıyor (kural ve dışlamalar SECURITY §3.7).
**Yerel gerçek koşu (ebeveyn, izin sonrası):** 98 vaka, 96 OK + 2
INFORMATIONAL, `autobahn.py check` geçti; ilk yazılan `0.8.2` etiketi
Docker Hub'da yoktu, `25.10.1`'e düzeltildi. Ajan turunda (izin
öncesi) doğrulananlar:
elle yazılmış istemciyle Autobahn biçimli 69 vaka yeşil, CI'nın kabuk
adımları, `check`'in sentetik raporlarla davranışı, YAML parse.

**Elenenler:** tam `gsb-server`'ı Autobahn'a hedeflemek (echo vakaları
sözleşme gereği FAIL olurdu), harness'e ayrı okuyucu yazmak (test edilen
kod üretim kodu olmazdı), opak modu cargo feature'ının arkasına koymak
(varsayılan lint/test örneği derlemezdi). Kararların gerekçeleri SECURITY
§3.7'de.

## gsb-kit Faz 5 turu (kit düzeltme turu — demoların bulguları)

`docs/KIT-ARCHITECTURE.md` §10 "Faz 5 sonucu": iki kontrol demosunun
"kit'e dokunma" kuralıyla kaydettiği tasarım bulgularının (F1–F4, A1–A2)
ve arena turunun opcode gözleminin (A3) hepsi kit'te kapandı — her biri
kendi commit'inde, önce kırılan testiyle, mutation-check'li. Demolar
düzeltmeleri kullanıyor; bulguları hatalı davranışla sabitleyen
testler çevrildi. **`gsb-core`'a dokunulmadı**; 2D demo'nun ve iki
3D demonun bayt kilitleri dokunulmadan yeşil.

**Commit'ler** (`32118b2..`): `c8ed9ba` F1, `fc4d10b` F2, `deb1d4e` F3,
`cfc51dc` F4, `c076fd0` A1, `7356bd7` A2, `2d39767` A3, ardından bu
doküman commit'i.

**Davranış değişiklikleri (açıkça):**
- **F1 — doğruluk düzeltmesi (sharded × spatial, 2D demo'nun bu yolu
  dahil):** şeritten ödünç verildiği hücreye göç eden entity artık yeni
  shard'ının kovasından silinmiyor. Önce: varış tick'inin one-shot
  full'u gelen oyuncunun kendisini içermiyordu; hücresinde yalnız bir
  entity o hücrede kıpırdayana dek yeni shard'ında görünmüyordu (komşu
  gruplara `cell_exits` gidiyordu). Farklı hücredeki bayat ödünç kopya
  eskisi gibi çıkarılıyor. Wire biçimi aynı; akış artık doğru.
- **F2 — yalnız ekleme:** `GridPartition2::with_diagonals()`
  (8-komşuluk). Varsayılan 4-komşuluk değişmedi (sunucunun demo
  odaları aynı).
- **F3 — belge + debug denetimi:** `Planar` / `GridPartition2` birim
  sözleşmesi yazılı; `Partition::debug_check_wire` (varsayılanlı, boş)
  — `GridPartition2` debug build'de wire izdüşümü konumdan bir şerit
  genişliğinden uzaksa panikliyor. Release'de davranış aynı; debug'da
  birimi yanlış bir oyun artık ilk ihraçta panikliyor.
- **F4 — varsayılan politika DEĞİŞMEDİ:** her odada
  `with_disconnect_policy(grace: Option<Duration>, to: ExpireTo)` ve
  `Game::may_release` (varsayılan `true`, her oda iletiyor). Varsayılan
  hâlâ 30 sn → AI devri; `with_disconnect_grace` anlamını koruyor.
  MMO asıl çıkış sayacına geçti (20 sn bekletme, sonra slot bırakılır).
- **A1 — varsayılanlı ekleme:** `TeamGame::spawn_team_player(world,
  conn) -> (Entity, Team)`, takım odası her katılımda onu çağırıyor;
  varsayılanı eski iki adım. İnce sözleşme farkı: `team_of` artık kit
  wire kimliğini damgalamadan ÖNCE soruluyor (hiçbir kit oyunu orada
  kimliği okumuyordu). Arena ezdi, `HomeBase` kalktı.
- **A2 — yalnız yorum:** istemci kuralları `kit.proto`'da; demo'nun
  `game.proto`'su ve 3D demoların aynaları oraya atıf yapıyor.
- **A3 — OYUN YAZARI İÇİN KIRICI:** `Game::SNAPSHOT_OP` /
  `PRIVATE_OP` varsayılansız; sabitleri adlandırmayan `impl Game`
  derlenmez (E0046). Üç demo zaten açıkça bildiriyordu — bayt değişmedi.

**Testler (+19 → 497):** kit — `sharded/tests/lent_arrival.rs` (4, F1),
`sharded/tests/diagonals.rs` (4, F2), `space/tests/units.rs` (4, F3),
`common/park/tests.rs` (2, F4: altı odanın hepsi), `team/tests/
spawn_team.rs` (2, A1), `Game`'in iki doctest'i (A3); MMO —
`reconnect::an_ai_handover_policy_hands_the_character_to_the_logout_bot`
(+1). **Bilerek değiştirilen iddialar:** MMO `findings::f1_…` /
`f2_…` çevrildi; `crossing`'in F1 izinleri kalktı (varış tick'inde
kendini kaybetme izni; ışınlanmada "bir hücre yürü" adımı) ve
ışınlanma tek adım (yolda 2 → 1 tick, F2); `reconnect`'in süre-dolumu
testi çıkışı iddia ediyor (F4 kilidi `(0, 2)` → despawn 1 / AI 0,
slot bırakıldı); arena birim testleri `spawn_team_player` çağırıyor
(iddialar aynı). Ayrıntı: KIT-ARCHITECTURE §10 "Faz 5 sonucu".

**Doğrulama:** `cargo test --workspace` → 497 passed / 0 failed / 1
ignored; clippy `-D warnings` 0; fmt temiz; kapanış kontrolü `cargo test
-p gsb-demo -p gsb-demo-arena -p gsb-demo-mmo` → 83 / 0 (43 + 15 + 25);
`git diff 32118b2.. -- crates/gsb-core` boş. Loadgen (50 istemci,
`32118b2` ↔ `2d39767`, dönüşümlü üç çift; hepsinde `left=50 errors=0
server_closes=0`) step_p50_fine_us taban / HEAD: tcp 56 40 32 / 56 40
24, spatial 96 112 112 / 88 88 112, sharded N=4 40 48 40 / 40 40 32,
sharded × spatial N=4 80 80 64 / 88 56 56 — gürültü içinde (snap_total
ve out_bps_per_conn ±%1).

## gsb-kit Faz 4 turu (3D MMO demosu `gsb-demo-mmo` — kapanış doğrulaması)

`docs/KIT-ARCHITECTURE.md` §10'un Faz 4'ü ve §13: kit'in yalnız public
yüzeyiyle yazılmış küçük bir 3D MMO dünyası, görünürlük modeli
**shard'lı dünya üzerinde yer-düzlemi ızgara AOI** —
`ShardedSpatialRoom<MmoGame, GridPartition2<Pos3>, Grid2>`, 2D
ön-ayarlar 3D verinin yer düzleminde (`Planar` = `[x, z]`; yükseklik
var, ilgi yönetimi onu yok sayıyor). **Beş korunan crate'e tek satır
dokunulmadı** (`git diff 89049d2.. --stat -- crates/gsb-kit
crates/gsb-core crates/gsb-demo crates/gsb-demo-arena crates/gsb-server`
boş; kit `addbcf5`'ten beri aynı); MMO sunucuya ve loadgen'e bağlanmadı
(§12 kapsam dışı).

**Ne yapıldı** (`6d20fb4`, `0aabb72`; ~1 470 satır kaynak (birim testleri dahil),
~1 280 satır entegrasyon testi, proto).
- **Crate `gsb-demo-mmo`** (workspace üyesi; `gsb-kit` + `gsb-core` +
  `gsb-protocol`'e bağlı, iki demoya da değil; `build.rs`
  `gsb_lint::check`'i çağırıyor, kit proto'sunu `links` üzerinden
  import ediyor).
- **Oyun:** `Pos3` (metre, y yukarı), `Vitals` (tür + can),
  `MmoCodec` (`Wire`: en yakına yuvarlanmış desimetre + tür + can —
  dünyanın her koordinatı ≤ 2 baytlık varint; `Dirty` = konum VEYA can;
  wire `Planar`'ı metre), 2×2 shard (512 m bölge, 128 m şerit), 64 m
  AOI hücresi; oyun kodunun spawn/despawn ettiği mob'lar (kamp
  tablosu, kendi hızıyla rota — `Speed` benzeri bileşen yok —, ömür,
  saldırıyla ölüm), uçan mob'lar (irtifa), `MoveTo` / `Attack` /
  `Travel` (waystone ışınlanması) girdileri, `MmoMig` (oyuncu: konum,
  can, hız, yürüyüş; mob: bütün beyni), park politikası (bekletme →
  AI devri: çıkış botu karakteri en yakın waystone'a yürütüyor).
- **Wire:** `proto/mmo.proto` — girdiler, `Kind`, `EntityRecord`,
  yer-düzlemi `CellExit { x, z }`, kit zarfının tipli aynaları;
  opcode'lar `1200..=1204` (2D demo ve arenadan ayrık blok).
- **Testler (+24):** 12 birim + 12 entegrasyon; entegrasyonların hepsi
  **gerçek dört `ShardActor`** üzerinden (registry kablolaması, elle
  ticker, metrik kanalı adım bariyeri, kit'in istemci kurallarıyla
  çözen istemciler): yer-düzlemi AOI (tam 3×3 blok, şerit dahil;
  150 m yukarıdaki komşu-hücre uçanı görünür, iki hücre ötedeki zemin
  mob'u görünmez), dikişi geçen oyuncu ve hızsız uçan mob (wire id +
  taşınan durum + iki yandan kesintisiz görünürlük), köşegen shard'a
  ışınlanma (hop hop, tek sahip), oyun kodunun öldürdüğü mob'un iki
  shard'dan kalıcı silinmesi (keep-alive full'larında hayalet yok),
  park / AI devri / sıfır süre, gerçek karelerin bayt uyumluluğu (full,
  `removed`'lı ve `cell_exits`'li delta, one-shot private full, ack).
  Mutation-check'ler: `Planar` `[x, y]`, `capture` mob'u oyuncu sayıyor,
  `Mig` yürüyüşü taşımıyor, ışınlanma yürüyüşü iptal etmiyor, öldürme
  despawn etmiyor, `Dirty` yalnız konum, sessiz bot, aynada
  `entities = 6`, wire `Planar`'ı desimetre — hepsi kırıldı (ayrıntı:
  KIT-ARCHITECTURE §10 "Faz 4 sonucu").
- **Kapanış kontrolü:** `cargo test -p gsb-demo -p gsb-demo-arena -p
  gsb-demo-mmo` → 82 passed / 0 failed (43 + 15 + 24), aynı ve Faz
  2'den beri değişmemiş kit üzerinde.

**Tasarım bulguları** (kit yamanmadı; en küçük değişiklikleri ve
kit-kopyası probları KIT-ARCHITECTURE §10 "Faz 4 sonucu"nda):
(F1, **doğruluk hatası**) sharded × spatial'da şeritten ödünç verilmiş
hücresine göç eden entity yeni shard'ının kovasından siliniyor —
varış full'u geleni içermiyor, hücresinde yalnızsa süresiz görünmez
(`integrate_borrowed`'ta ~6 satırlık kit-içi düzeltme; probda 125/126,
kırılan tek test bilerek sabitlenen bulgu testi); (F2) `GridPartition2`
4-komşuluk: köşegen shard köşeden hiçbir şey ödünç vermiyor
(`with_diagonals()` eklemesi); (F3) `Planar`'ın birim sözleşmesi yazılı
değil (belge); (F4) park politikası her beklemeyi AI devrine bitiriyor —
"sonra slotu bırak" ifade edilemiyor (`ParkPolicy.to` eklemesi). F1 ve
F2 `tests/findings.rs`'te bugünkü davranışla sabit (kit düzelince
bilerek kırılır). Faz 3 + 4'ün birleşik bulgu listesi (kit düzeltme
turunun girdisi): KIT-ARCHITECTURE §10 "Faz 3 + Faz 4 tasarım
bulguları".

**Elenen alternatifler.**
- *Kendi `Partition`'ını yazarak köşe açığını kapatmak:* public seam
  izin veriyor, ama turun konusu ön-ayar; bu, F2'nin etrafından sessiz
  bir dolaşma olurdu.
- *Kit odasını saran bir `GameLogic` ile park cevabını `Despawn`'a
  çevirmek:* her kanca için delege — kit'in iç davranışını oyunda
  yeniden yazmak; F4 olarak kaydedildi.
- *Santimetre / `f32` wire:* ±81,9 m ötesi her koordinat 3 bayt / 5
  bayt; desimetre MMO ölçeğinde yeterli ve değişim eşiği bedava.
- *Wire `Planar`'ını desimetre bırakmak:* `Grid2`'nin hücre kenarını
  640 yapmak yeterdi, ama `GridPartition2::admits` konumun birimini
  bekliyor (F3) — iki izdüşüm tek birimde.
- *Mob'lara `Speed` vermek:* §8.5'in sınanması için bilerek yok; hız
  mob'un beyninde.
- *MMO'yu sunucuya / loadgen'e bağlamak:* §12 kapsam dışı; en küçük
  sunucu kancası değerlendirmesi §10 "Faz 4 sonucu"nda.

**Test:** 454 → **478** (+24). Clippy 0 uyarı. Loadgen (50 istemci, 3
sn) `left=50 errors=0` — çalışma zamanında değişen kod yok, A/B
alınmadı.

## gsb-kit Faz 3 turu (3D arena demosu `gsb-demo-arena` — kit'in kabul testi)

`docs/KIT-ARCHITECTURE.md` §10'un Faz 3'ü ve §11'in ilk kabul
kriterinin ikinci demosu: kit'in yalnız public yüzeyiyle yazılmış bir
3D takım arenası, görünürlük modeli **3D takım sisi** (yükseklik
sayılır), üç takım. **`crates/gsb-kit` ve `crates/gsb-core`'a tek satır
dokunulmadı** (`git diff addbcf5.. --stat -- crates/gsb-kit
crates/gsb-core crates/gsb-demo crates/gsb-server` boş); arena sunucuya
ve loadgen'e bağlanmadı (§12 kapsam dışı).

**Ne yapıldı** (`3fc92ef`, `a36a7c7`; ~760 satır kaynak + ~650 satır
test + proto).
- **Crate `gsb-demo-arena`** (workspace üyesi, `[workspace.dependencies]`
  girdisi; `gsb-kit` + `gsb-core` + `gsb-protocol`'e bağlı, `gsb-demo`'ya
  değil; `build.rs` `gsb_lint::check`'i çağırıyor).
- **Oyun:** `Pos3` (metre, y yukarı, `Spatial`), kendi 3D kinematik
  hareket sistemi (dikey dahil), `ArenaCodec` (`Wire = Cm3`: en yakına
  yuvarlanmış tam sayı santimetre — arenanın her koordinatı ≤ 2 baytlık
  varint), `ArenaGame: Game + TeamGame` (üsse spawn, katılım sırasıyla
  round-robin üç takım, `MoveTo` girdisi kit'in `InputSeq`'i altında,
  üssüne çekilen bot), oda `TeamRoom<ArenaGame, VisionGrid3<Pos3>>`
  (yarıçap 15 m).
- **Wire:** `proto/arena.proto` — `MoveTo` (cm + `seq`), `UnitRecord`,
  kit zarfının tipli aynaları (`WorldSnapshot`, `Private`); kit
  proto'su demo'daki gibi `links` üzerinden import ediliyor. Opcode'lar
  `1100..=1102` (demo'nunkilerden ayrık blok).
- **Testler (+15):** 8 birim (nicemleme, kayıt gövdesi, round-robin,
  üs mesafeleri, bot, 3D hareket ×2, opcode bandı) + 7 entegrasyon,
  gerçek `RoomActor` üzerinden: üç takımın her üyesi tam olarak
  takımının gördüğünü alıyor; paylaşılan takım görüşü; tam üstteki
  yarıçap-dışı düşman gizli, yarıçap-içi görünür (2D sisin geçemediği
  test); hareket görünürlüğü tick tick değiştiriyor; girdi sıra/ack;
  kit zarfı ile aynanın bayt uyumluluğu (kurulmuş ve gerçek kareler —
  gerçek kareler iki tanımda da kit'in yazdığı baytların aynısına
  yeniden kodlanıyor). Mutation-check'ler: `Spatial`'de yükseklik
  terimi yok / `VisionGrid2` ile değiştirme → yükseklik testleri;
  iki takımlı parite / tek takım / herkese ayrı takım → sis testleri;
  dikeyi yok sayan hareket → yükseklik, girdi, wire; sıra kuralını
  yok sayan `ingest` → girdi; aynada `entities = 6` → wire (ayrıntı:
  KIT-ARCHITECTURE §10 "Faz 3 sonucu").

**Tasarım bulguları** (engelleyici değil; kit'e değişiklik
gerekmedi): (1) oda takımı spawn'dan SONRA soruyor — takıma bağlı
spawn noktası için arena takımı `spawn_player`'da seçip `HomeBase`'e
yazıyor, `team_of` geri okuyor; en küçük düzeltme `TeamGame`'e
varsayılanlı `spawn_team_player` (yalnız ekleme). (2) Kit zarfının
istemci kuralları `kit.proto`'da değil demo'nun `game.proto`'sunda
yazılı (belge bağlılığı; düzeltme yalnız yorum). Gözlemler: kit'in
opcode varsayılanları demo'nun numaraları; `Vision::sees` birim başına
yarıçap taşımıyor; takım odası yalnız full gönderiyor — hiçbiri arenayı
engellemedi. KIT-ARCHITECTURE §10 "Faz 3 sonucu".

**Elenen alternatifler.**
- *Takımı conn modülüyle atamak* (demo'nun kuralı): oturum kimlikleri
  sunucu-geneli, bir odanın katılanları aynı kalanı paylaşabilir.
- *En küçük takımı doldurmak:* ayrılış bir park (birim ve takımı
  kalıyor); katılım başına dünya taraması karşılığında kadro pek
  değişmiyor.
- *Takımı `team_of`'ta seçip birimi sonra üsse taşımak:* `team_of`
  `&World` alıyor (taşıyamaz); bir sonraki tick'te taşımak birimi bir
  tick yanlış yerde yayınlardı.
- *Milimetre / desimetre / `f32` wire:* §10 "Faz 3 sonucu"nun codec
  satırı.
- *`Planar` da uygulamak:* arena hiçbir yer-düzlemi ön-ayarı
  kullanmıyor (`VisionGrid2` yalnız mutasyon probunda, geçici).
- *Arenayı sunucuya / loadgen'e bağlamak:* §12 kapsam dışı.

**Test:** 439 → **454** (+15). Clippy 0 uyarı. Loadgen (50 istemci, 3
sn) `left=50 errors=0` — çalışma zamanında değişen kod yok, A/B
alınmadı.

## gsb-kit Faz 2 turu (crate bölmesi: `gsb-kit` + `gsb-demo`, kit proto'su, `Spatial` + `VisionGrid3`)

`docs/KIT-ARCHITECTURE.md` §10'un Faz 2'si: workspace'te artık
`crates/gsb-kit` (stratejiler, delta motoru, sharded kompozitler,
park/resume, ön-ayarlar, kendi proto'su) ve `crates/gsb-demo` (yeniden
adlandırılan `gsb-game`, örnek oyun). **Kit hiçbir profilde demo'ya
bağlı değil** (`cargo tree -p gsb-kit -e normal,dev,build | grep -c
gsb-demo` → 0). **Wire baytları aynı:** `wire_contract.rs`,
`delta_aoi.rs` ve diğer entegrasyon testleri yalnız `gsb_game` →
`gsb_demo` ve bir `prelude` import'uyla taşındı; demo kodeğinin değerine
bakan 11 modül içi test demo'ya aynı ad ve assertion'larla gitti.
`gsb-core` dokunulmadı; `gsb-server`'da yalnız yol değişiklikleri.

**Ne yapıldı.**
- **`gsb-game` → `gsb-demo`** (`git mv`; paket `gsb-demo`, lib
  `gsb_demo`, açıklama "example game for gsb-kit"). Public yollar aynı
  (tip takma adları ve yeniden ihraçlar).
- **Kit proto'su** (`gsb-kit/proto/kit.proto`, `gsb.kit`):
  `WorldSnapshot` (opak `repeated bytes` kayıt ve hücre gövdeleri),
  `InputAck` ve `Private` — oyunun kendi özel yükü için yeni `bytes
  game = 4` yuvasıyla (4 hiç kullanılmamıştı; hiçbir oda henüz
  yazmıyor). `build.rs` diğerlerinin kalıbında, proto dizinini `links =
  "gsb-kit-proto"` ile yayınlıyor. Demo'nun `game.proto`'su onu import
  ediyor: `InputAck` kit'in (`gsb_demo::game::InputAck` yeniden ihraç),
  `WorldSnapshot` / `Private` demo'nun tipli aynası (istemciler
  değişmedi). Ayrıntı: KIT-ARCHITECTURE §5.1.
- **Bayt uyumluluğu crate sınırında kilitli:** kit tarafında alan
  numaraları bayt bayt (`proto::tests` ×2); demo tarafında
  `tests/kit_wire.rs` ×2 — kurulmuş kareler (full; `removed` +
  `cell_exits` + kayıtlı delta; ack + yanıtlı ve one-shot full +
  yanıtlı `Private`) iki tanımda aynı baytları kodluyor ve birbirini
  kendine çözüyor; gerçek bir demo `AoiRoom`'unun yazdığı kareler iki
  tanımdan aynı içeriğe çözülüp aynı baytlara yeniden kodlanıyor.
- **Kit testleri kendi fikstür oyununda** (`gsb-kit/src/testing/`,
  yalnız testlerde): konum / hız / hedef, kesen bir kodek, conn
  paritesiyle takım, göç durumu, dört sektörlü harita, kit zarfının
  prost-derive tipli aynaları, altı oda için kurucular. Seam boşaldı ve
  silindi.
- **Demo kurucuları uzantı trait'leri** (`OpenRoomExt` … 
  `ShardedSpatialRoomExt`, `gsb_demo::prelude`): kit tiplerindeki
  inherent impl crate sınırında E0116'dır; çağrı sözdizimi aynı kaldı.
- **Crate bölmesi** (`git mv` ile `src/kit/*` → `gsb-kit/src/*`):
  `crate::kit::` → `crate::`, kapsamlar aynı. Derleyicinin bulduğu iki
  public-yüzey ihtiyacı: `InputSeq` (`Game::ingest`'in imzasında, özel
  modüldeydi) `gsb_kit::game`'den ihraç ediliyor; `run_systems` demo'ya
  taşındı (kit `gsb-ecs`'e bağlı değil).
- **Katman testi emekli, yerine yapısal kural:** `layering.rs` (2 test)
  kalktı — normal bir kit → demo bağımlılığı cargo döngüsü (derlenmez);
  cargo'nun kabul ettiği dev-dependency döngüsünü kit'in manifest testi
  yakalıyor (mutation-check'li).
- **3D ön-ayar:** `Spatial` erişimcisi (`[Coord; 3]`), `Cell3`,
  `VisionGrid3<P>` (27 hücrelik komşuluk, kesin 3D mesafe) — arenanın
  2'den fazla takımlı savaş sisi için. `Grid3` ve `GridPartition3`
  kurulmadı (tetikleyici bekliyor, §7).

**Bulunan hata.** `aoi_join_leave_same_tick_inert` katıldığı oyuncuyu
değil `PlayerId(9)`'u bırakıyordu (katılım `PlayerId(2)` basar):
ayrılış hiç olmuyordu, test demo'nun spawn dağılımı sayesinde
geçiyordu. Fikstür orijine spawn edince kırıldı; artık katılımın
oyuncusunu bırakıyor (e80d2e4'ün kodunda da geçiyor). Assertion'lar
aynı.

**Test:** 433 → 439 (−2 emekli katman testi, +8: kit zarfı alan
numaraları ×2, `kit_wire` ×2, kit manifest ×1, `VisionGrid3` ×2, üç
takımlı 3D takım sisi ×1). Taban'daki 96 `gsb-game` testinin her
birinin yeni yeri: KIT-ARCHITECTURE §10 "Faz 2 sonucu".

**Loadgen** (50 istemci; `e80d2e4` ↔ HEAD `c558949` ikilileri, dönüşümlü
üçer çift; sonraki commit'ler yalnız doküman). Her koşuda `left=50`,
`errors=0`, `server_closes=0`:

| Koşu | step_p50_fine_us (taban / HEAD, 3 çift) | snap_total (taban / HEAD) | out_bps_per_conn (taban / HEAD) |
|---|---|---|---|
| tcp 3 sn | 56 64 128 / 64 72 112 | 4100 / 4100 | 11026 / 10996 |
| udp 3 sn | 56 64 128 / 56 80 56 | 4150 / 4100 | 10987 / 11023 |
| spatial 3 sn | 120 120 160 / 112 128 152 | 4085 / 4094 | 1924 / 1974 |
| team 3 sn | 72 80 88 / 80 96 96 | 4100 / 4100 | 11066 / 11021 |
| pvs 3 sn | 80 88 88 / 80 112 40 | 4100 / 4141 | 6095 / 6072 |
| sharded N=4, 8 sn | 56 104 40 / 48 80 48 | 11474 / 11476 | 7192 / 7080 |

snap_total / out_bps ilk çiftin değerleri. Üçüncü çiftte tcp ve udp
tabanı birlikte 128'e sıçradı (makine yükü; aynı çiftte HEAD 112 / 56).
`team`'in ilk üç çiftinde HEAD bir-iki kova yukarıdaydı; altı çift daha
alındı: taban 96 88 88 72 80 64, HEAD 96 80 72 72 72 80 — dokuz çiftte
ortalama 80,9 / 82,7 µs (bir kovanın dörtte biri), işaret çiftten çifte değişiyor; 10
sn'lik dört çift de karışık (taban 56 80 272 88, HEAD 64 56 96 176; iki
koşu makine yüküyle sıçradı). Çalışma zamanında değişen kod yok (kayıt
ve zarf yolları aynı; tek fark `emit_private`'ın boş `game` alanı) —
gürültü içinde sayıldı.

**Elenen alternatifler.**
- *Paylaşım/delta testlerini topluca demo'ya taşımak:* bir kısmı kit'in
  özel defterine bakıyor (`born_groups`, `member_counts`,
  `pending_removals`, `conn_view`); taşımak alan görünürlüğünü
  genişletmek demekti. Ölçüt test başına: demo kodeğinin yazdığı değere
  bakan demo'ya, bakmayan fikstüre.
- *Fikstürü yalnız kodek düzeyinde tutup testleri demo'da çalıştırmak:*
  kit'in özel alanlarına erişimi kaybettirir; kit kendi davranışını
  kendi crate'inde test etmeli.
- *Fikstürü public bir `testing` modülü (ya da cargo feature'ı) yapmak:*
  oyun yazarına örnek zaten `gsb-demo`; public bir fikstür oyunu
  API yüzeyi ve bakım yükü, bugün bir kullanıcısı yok.
- *Kit'e `gsb-demo`'yu dev-dependency yapmak:* cargo buna izin verir ama
  kit'in ikinci bir kopyasını derler (testteki tipler demo'nun tipleri
  olmaz) ve katman kuralını tam da test profilinde deler.
- *Demo kurucuları için serbest fonksiyonlar / newtype'lar / kit'te
  generic kurucular:* §10 "Faz 2 sonucu".
- *`kit.proto`'da `removed` için `[packed = false]`:* kit'in elle yazdığı
  biçimi betimlerdi ama üretilmiş kodlayıcılarda demo'nun (değişmeyen,
  packed-varsayılan) aynasından farklı bayt üretirdi; ayrıştırıcılar
  ikisini de okuyor.
- *Demo aynasına `Private.game = 4`'ü eklemek:* demo göndermiyor, ve
  `wire_contract.rs`'in `Private` literal'ini değiştirmek gerekirdi.
- *`Grid3` / `GridPartition3`'ü şimdi kurmak:* kontrol demolarından
  hiçbiri kullanmıyor (tetikleyicisiz iş yapılmaz).

**Tasarım bulguları (KIT-ARCHITECTURE §10 "Faz 2 sonucu"):** §12'nin
arena için "`Grid3` (27 hücre)" dediği ızgara bir `CellSpace` değil
bir `Vision` ön-ayarı (`VisionGrid3`); `InputSeq`'in public yolu yoktu
(crate içinde fark edilmez, crate sınırı yakaladı); demo'nun kurucuları
yetim kuralına takıldı (1a'dan beri kayıtlı tuzak).

## gsb-kit Faz 1b turu (takım sisi, PVS, sharded kompozitler generic; §8.2–§8.5; seam kit zarfına indi)

`docs/KIT-ARCHITECTURE.md` §10'un Faz 1'inin ikinci yarısı — **Faz 1
bitti**: her oda oyun üzerinden generic, kit demo'ya yalnız kendi zarf
tipleri için ulaşıyor. **Wire baytları aynı**: `wire_contract.rs`,
`delta_aoi.rs`, `aoi/tests/sharing*.rs` ve takım / PVS / sharded
içerik testleri tek bir beklenen bayt, kayıt sayısı ya da assertion
değişmeden geçti (`crates/gsb-game/tests/` hiç değişmedi; modül içi
test dosyalarına yalnız tip takma adları, çocuk modül satırları ve elle
kurulan iki göç durumunun yeni şekli eklendi). `crates/gsb-game`
dışında hiçbir dosya değişmedi.

**Ne yapıldı.**
- Seam'ler (derlenen imzalar ve sapmalar: KIT-ARCHITECTURE §4.6):
  `Vision` + `VisionGrid2<P>`, `SectorMap` + `ConvexSectors2<P>`
  (`in_convex` demo'dan kit'e döndü), `Partition<W>` +
  `GridPartition2<P>` (`grid_shape`, `shard_at` dahil), `TeamGame`
  (takım ataması), `ShardGame` (`Mig`, `capture`, `restore`),
  `KitMig<M>` (oyunun durumu + kit'in park kaydı).
- **Konum erişimcisi `Planar`:** 2D ön-ayarların hepsi (`Grid2` dahil)
  oyunun konum ve wire tiplerini `planar() -> [Coord; 2]` üzerinden
  okuyor; yer düzleminde (x, z) yaşayan bir 3D oyun ön-ayarları kit'e
  dokunmadan kullanır (testle kilitli). Demo'nun `Wire`'ı `StripPos`
  oldu (`Strip = Wire`).
- Odalar: `TeamRoom<G, V>`, `SectorRoom<G, M>`, `ShardedRoom<G, P>`,
  `ShardedSpatialRoom<G, P, S>`. Join / girdi / sistemler / istek
  yolları kit'in ortak yolları; snapshot zarfları kit'in
  (`write_full_header` + kodek). Sharded park kopyası ortak `park.rs`
  ile birleşti (§4.4): aynı defter ve kanca gövdeleri, göçte kayıt
  `KitMig` içinde taşınıyor.
- Eski yollar aynı: `gsb_game::{team::TeamRoom, pvs::SectorRoom,
  sharded::{ShardedRoom, ShardedSpatialRoom, ShardedRoomState}}` demo
  örneklemelerine tip takma adı (`ShardedRoomState = KitMig<DemoMig>`),
  kurucular `demo/rooms.rs`'te; `TEAM_COUNT` demo'nun ataması oldu
  (yol aynı).
- Seam: test fikstürleri dışında yalnız kit zarfı (`Private`,
  `private::Payload`, `InputAck`) kaldı — Faz 2'de kit proto'suna.

**Davranış değişiklikleri (bilinçli, her biri kendi testiyle).**
1. **Tüm kit odaları istekleri oyuna yönlendiriyor** (ebeveyn kararı):
   AOI, takım ve PVS odaları artık `Game::handle_request`'e soruyor.
   Demo'da bu, o stratejilerde `ABILITY`'nin cevaplanması ve
   `ECONOMY`'nin "no handler" yerine demo'nun normal "economy service
   not configured" reddiyle dönmesi demek (fabrikaları economy servisi
   bağlamıyor). Oyunun işleyicisi yoksa (`None`) çekirdek yine "no
   handler" der.
2. **§8.4 takımlar:** üçüncü bir takım rebuild'de dizinin dışına
   taşıp paniklerdi; artık oyunun atadığı kadar takım (en çok 256).
3. **§8.4 sektörler:** 16'dan fazla sektörlü harita yapımda taşma
   paniği verirdi (release'te sessizce yanlış görünürlük); artık 255
   sektöre kadar (anahtar `u8`), daha fazlası için oyun kendi
   `SectorMap`'ini yazar.
4. **§8.4 bitişik olmayan göç:** sahibinin komşusu olmayan bir bölgeye
   düşen entity (2×2'nin köşegeni) hiçbir yere raporlanmaz ve yanlış
   sahipte kalırdı; artık en kısa yolun ilk komşusuna verilip adım adım
   (adım başına bir tick) sahibine ulaşıyor.
5. **§8.5:** `Speed`'siz bir entity hiç göç etmezdi; artık `Marker`'ı
   taşıyan her entity göç ediyor, `Speed`'i yoksa `Speed`'siz kuruluyor.
6. **§8.2:** oyun kodunun despawn ettiği entity AOI ve spatial kompozit
   defterinde hayalet kalırdı (grup çıkış almaz, keep-alive / one-shot
   full'lar onu taşımaya devam ederdi); artık grubun delta'sı onu
   çıkarıyor, shard tabloları da unutuyor. Aynı commit: göçle çıkan bir
   NPC'nin hücresi artık üye sayısından düşülmüyor (eskiden bir oyuncuyla
   paylaştığı hücrenin sayısını sıfırlıyordu).
7. **§8.3:** takım, PVS ve düz sharded oda `clear_trackers`'ı hiç
   çağırmıyordu (silinen-bileşen tamponları odanın ömrü boyunca
   büyüyordu); artık her oda tick başına bir kez kapatıyor. Wire'a
   etkisi yok.
8. PVS'te yalnız `Marker`'ı olan (harita konumu olmayan) bir entity
   sınırlama sektörüne düşüyor (eskiden hiç kovalanmazdı); demo'da
   `Marker` konumun kendisi olduğu için gözlemlenemez.

**Test:** 419 → 433 (+14: 3 takım, 20 sektör, `Speed`'siz NPC göçü,
köşegen göç yönlendirmesi, yönlendirme tablosu özelliği, üç oda için
değişiklik penceresi, AOI ve spatial kompozit hayaletleri, göçle çıkan
NPC'nin üye sayısı, altı odada istek yönlendirmesi, 3D yer düzlemi
ön-ayar kilidi, demo'nun `SECTOR_OUT`'u = sınırlama sektörü). Her §8
düzeltmesi önce kırılan testiyle, ayrı commit'te. Loadgen tcp / udp /
spatial / team / pvs / sharded / sharded × spatial, taban ↔ HEAD
dönüşümlü üçer çift: `left=50`, `errors=0`, `server_closes=0` her
koşuda, snap_total / out_bps gürültü içinde; `spatial`'in adım p50'si
için dokuz çift alındı, ortalama fark bir kova (tablo ve ayrıntı:
KIT-ARCHITECTURE §10 "Faz 1b sonucu"). Geçici bir eşdeğerlik koşumu
(yalnız public API; commit'lenmedi) altı stratejinin 900 tick'lik
rastgele senaryodaki bütün çıktısını taban ve HEAD'de kanonik olarak
birebir aynı buldu.

**Elenen alternatifler.**
- *`Mig` / `capture` / `restore`'u taslaktaki gibi `Game`'e koymak:*
  kararlı Rust'ta ilişkili tip varsayılanı yok; shard'lanmayan her oyun
  ölü bir `Mig` ve iki ölü kanca yazardı. `ShardGame: Game` uzantısı.
- *Takım ataması için genel `on_player_spawned`:* her odada çağrılır,
  demo takımsız odalarda da `TeamMember` yazardı. `TeamGame::team_of`.
- *Bitişik olmayan göçte entity'yi sahip shard'a doğrudan vermek:*
  çekirdek yalnız komşulara link tutuyor ve `collect_migrations`'ı
  yalnız `neighbors()` için çağırıyor; `neighbors()`'ın bütün shard'ları
  döndürmesi sınır değişimini herkes-herkese çevirirdi (davranış + CPU).
  Çekirdek dokunulmaz → adım adım yönlendirme.
- *`GridPartition2`'ye köşegen komşuluk (8-komşu):* yalnız ızgaranın
  köşegenini çözer, sınır ortaklarını ikiye katlar; BFS yönlendirmesi
  her topolojide çalışıyor.
- *Ayrılış / göç çıkışını da silinen-tampon süpürmesine bırakmak (park
  etmeyi kaldırmak):* ayrılışın üyeliğini ancak ayrılış anı biliyor ve
  aynı tick'teki ayrılış + göç çıkışının hücre içi çıkış sırası
  değişirdi; park etme kaldı, süpürme yalnız park edilmeyeni buluyor.
- *Takım sayısı için sabit (const generic) bir üst sınır, sektörler için
  daha geniş bir bit kümesi:* ikisi de yeni bir sabit sınır.
- *`SectorMap::visible_from -> &[Sector]`:* ön-ayarın gösterimini
  dilime zorlar; iteratör, bit maskesinden listeye geçişi imza
  değişmeden yaptı.
- Konum erişimcisinin elenen alternatifleri (izdüşüm tip parametresi,
  closure, somut `Pos2`/`Pos3`, `Planar<T>`, `Into<[f32; 2]>`):
  KIT-ARCHITECTURE §4.6.

**Tasarım bulguları (KIT-ARCHITECTURE §10 "Faz 1b sonucu"):** göç
yönlendirmesinde ara shard entity'yi bir tick boyunca kendi dünyasında
taşıyor (snapshot'ında ve sınır ihracında görünür); silinen-tampon
süpürmesi §4.4'ün "`clear_trackers`'ın tek sahibi kit" kuralını artık
doğruluk için taşıyıcı yapıyor; demo kurucuları hâlâ kit'in generic
tipleri üzerinde inherent impl (Faz 2 tuzağı, 1a'dan).

## gsb-kit Faz 1a turu (seam trait'leri, generic delta motoru, `OpenRoom<G>` / `AoiRoom<G, S>`)

`docs/KIT-ARCHITECTURE.md` §10'un Faz 1'inin ilk yarısı. **Wire baytları
aynı**: `wire_contract.rs`, `delta_aoi.rs`, `aoi/tests/sharing*.rs`
değişmeden geçti; `crates/gsb-game` dışında hiçbir dosya değişmedi.

**Ne yapıldı.**
- §4 seam'leri: `RecordCodec` (Marker / Query / Dirty / `Wire`,
  `wire`, `encode`), `CellSpace<W>` (+ kit'in `Grid2` ön-ayarı) ve
  `Game` (kodek, opcode'lar, `spawn_player`, `bot_actions`, `ingest`,
  `systems`, `handle_request`). Derlenen imzalar ve on sapma, gerekçeleriyle:
  KIT-ARCHITECTURE §4.5.
- Kimlik: `WireId::new` kalktı; tek inşa yolu kit-özel `Minter`
  (`Sequential` / `Range`). Sharded odanın üç doğrudan çağrısı (join,
  yetim damgası, göç girişi) ondan geçiyor — **§8.1 kapandı**, bir
  `compile_fail` doctest'i kilitliyor.
- Girdi sıra kuralı kit tipi oldu: `InputSeq` (oyun yalnız
  `admit(player, seq)` görüyor).
- Delta motoru generic: `CellBook<W, C>`, `CellPieces<C>`,
  `assemble_group_packet`; gövdeler oyundan (kodek, uzay), zarflar
  kit'ten (`common/frame.rs`, `put_delimited`: bir baytlık uzunluk
  yuvası, ortak durumda kopya yok). Kayıt kodlama sayısı değişmedi
  (değişiklik testi hâlâ tipli `Wire` karşılaştırması).
- `OpenRoom<G: Game>` ve `AoiRoom<G, S: CellSpace<Wire<G>>>`; ortak
  muhasebe `common/hooks.rs`'te kit ile `Game` kancaları arasında
  bölündü. Demo: `DemoGame`, `DemoCodec` (`Wire = (i32, i32)`), `Grid2`;
  eski kurucular `demo/rooms.rs`'te, eski yollar tip takma adı.
- Değişiklik takibinin tek sahibi kit (§4.4): iki oda da
  `clear_trackers`'ı `update` sonunda bir kez çağırıyor, bir kancanın
  çağırması debug'da yakalanıyor. **§8.3 `OpenRoom` için kanıtlandı ve
  kapandı:** oda bunu hiç çağırmıyordu, her ayrılışın despawn'ı odanın
  ömrü boyunca silinen-bileşen tamponunda kalıyordu (önce testle
  kanıtlandı, sonra düzeltildi).
- Seam: çevrilen iki oda seam'den hiçbir şey almıyor; kalan her öğe
  tüketicisiyle etiketlendi (= 1b iş listesi, KIT-ARCHITECTURE §10
  "Faz 1a sonucu").

**Test:** 411 → 419 (iki `Minter`, bir `compile_fail` doctest,
`put_delimited`, demo kodeki ↔ tipli `EntityRecord` ve `Grid2` ↔ tipli
`CellExit` bayt sabitlemeleri, `OpenRoom` değişiklik penceresi,
kanca bekçisi). Loadgen tcp / udp / spatial / sharded / sharded ×
spatial, taban ↔ HEAD dönüşümlü: gürültü içinde (tablo §10).

**Elenen alternatifler.**
- *`RecordCodec::encoded_len` (ya da `CellSpace` için eşi):* protobuf
  uzunluk önekini gövdeden önce bilmek için; ama gövdeyle tutarlı
  kalması gereken ikinci bir metot, bozulduğunda sessizce kırık çerçeve
  demek. `put_delimited` aynı baytları tek metotla ve ortak durumda
  kopyasız üretiyor.
- *Gövdeyi geçici bir tampona kodlayıp kopyalamak:* doğru ama kayıt
  başına bir kopya; bir baytlık yuva onu yalnız 128 baytı aşan (nadir)
  gövdelere bırakıyor.
- *`Minter` public bir enum:* varyant alanları public olan bir enum'u
  herkes kurar, yani istediği kimliği basar; tip kit-özel.
- *Göçle gelen kimlik için `WireId`'yi `ShardedRoomState` içinde
  taşımak:* çekirdeğin `Migrating::wire`'ı zaten ham `u64` (çekirdek
  dokunulmaz); ikinci bir kopya tutarsızlık kapısı olurdu —
  `Minter::arrival` tek, kit-içi yol.
- *`AoiRoom<G>`'yi `Game::handle_request`'e bağlamak:* AOI odası
  istek cevaplamaya başlardı — davranış değişikliği; karar bekliyor.
- *Kit'te demo tiplerine varsayılan tip parametreleri
  (`AoiRoom<G = DemoGame>`):* kit demo'yu adlandırırdı (§3); eski yollar
  bunun yerine kökte tip takma adı.
- *1b odalarının (takım, PVS, sharded) snapshot kodlayıcılarını da
  `DemoCodec`'e çevirmek (`EntityRecord`'u seam'den düşürmek için):*
  1b'nin odalarını başlatmak demekti; bilinçli olarak bırakıldı.
- *`Pos2` ön-ayarı:* 1a odalarının kullandığı tek kodek demo'nun;
  kullanılmayan bir ön-ayar kurulmadı.

**Tasarım bulguları (KIT-ARCHITECTURE §10 "Faz 1a sonucu"):** seam
sayıca küçülmedi (tüketicilerin hepsi 1b'de); `AoiRoom<G>`'nin istek
yönlendirmesi karar bekliyor; demo kurucuları kit'in generic tipleri
üzerinde inherent impl — Faz 2'nin crate bölmesinde serbest
fonksiyona ya da uzantı trait'ine dönmeli; `Grid2` `CellExit` gövdesini
kendisi yazıyor (demo testiyle sabit); katman tarayıcısı çıplak
`crate::kit` yolunu da kabul ediyor.

## gsb-kit Faz 0 turu (modül bölmesi: `kit/` + `demo/` + geçici seam)

`docs/KIT-ARCHITECTURE.md` §10'un ilk fazı. **Davranış değişikliği yok,
wire baytları aynı**; kod yalnızca `crates/gsb-game/src` içinde taşındı.

**Ne yapıldı.** `gsb-game/src` iki özel modüle ayrıldı. `kit/`:
stratejiler (`room`/`OpenRoom`, `aoi`, `team`, `pvs`, `sharded`), ortak
makine (`common`: hücre-delta motoru, park politikası, girdi sıra/ack
kuralı, `Private` çerçeveleme, yetim damgalama, basım) ve kit'e ait
`identity` (`WireId`). `demo/`: bileşenler, hareket sistemi, economy
servisi, opcode'lar, üretilen proto, `register`, spawn dağılımı, `MOVE_TO`
çözme, bot, RPC işleyicileri, PVS haritası, `StripPos`. Karışık
fonksiyonlar işlevine göre bölündü: `on_join` (kit: basım + tablolar;
demo: `spawn_player`), `ingest` (demo: çözme + uygulama; kit: sıra
kuralı `InputState::admit`), `handle_request` (iki birebir kopya →
`demo/rpc.rs`'te tek gövde), `on_migrate_in` (demo: `restore_migrant`),
takım ataması (`team_of` → demo). Taşımalar `git mv` ile; 8 kod
commit'i, her biri derleniyor ve tüm süit yeşil.

**Seam.** Kit'in demo'ya her erişimi tek modülden, `kit/seam.rs`'ten
geçiyor; içeriği hedef seam'e göre sıralı (RecordCodec / CellSpace /
Vision / SectorMap / Partition / Game / kimlik / kit zarfı / test
fikstürleri) ve **Faz 1'in iş listesi**: 25 öğe + 4 test-yalnız öğe.
Kural `src/layering.rs`'teki kaynak-tarayan testle kilitli: `kit/`
altında seam dışındaki her `crate::` yolu `kit::` ile devam etmeli
(yorumlar dahil). Envanter, temiz bölünmeyenler ve notlar:
KIT-ARCHITECTURE §10 "Faz 0 sonucu".

**Public API.** Eski yolların hepsi kökten yeniden ihraç ediliyor
(`gsb_game::room`, `::aoi`, `::team`, `::pvs`, `::sharded`,
`::components`, …); `crates/gsb-game` dışında hiçbir dosya değişmedi.

**Elenen alternatifler.**
- *Yalnız `grep crate::demo` kuralı:* kök uyumluluk yolları
  (`crate::room::spawn_pos`, `crate::components::Position`) demo'ya
  seam'i atlayarak ulaşmayı mümkün bırakırdı; kural "`crate::` yolu
  `kit::` ile devam eder" olarak sıkılaştırıldı.
- *`WireId`'yi demo'da bırakıp seam'den geçirmek:* §4.4 kimliği kit'e
  veriyor ve basım zaten kit'te; tipi taşımak saf bir taşıma, seam'de
  tutmak Faz 1'e geri taşınacak ~10 referans demekti.
- *`collect_migrations`'ı bölmek:* yakalama bölge sorgusuyla kaynaşık;
  bölmek sorgu filtresini (`&Speed` şartı, §8.5) ya da sırayı
  değiştirirdi — kit'te kaldı, tipleri seam'den.
- *Crate'leri şimdi bölmek:* §9 (kit demo'ya bağımlı olurdu).

**Yan bulgu (kapsam dışı, düzeltilmedi).** `gsb-server/tests/write_stall.rs`
`a_peer_that_stops_reading_loses_its_session` bir tam-süit koşusunda
bir kez kırıldı (`closes=1, conns=1`), tek başına üç koşuda geçti.
Sebep testte: registry `ConnClosed`'da `closes`'u artırıp metrik
yayıyor, bağlı satırı ise odanın sonraki `DetachDespawned`'ına kadar
tutuyor; test yalnız `closes >= 1`'i bekleyip `conns == 0`'ı iddia
ediyor. Ayrı iş olarak işaretlendi. *(Kapandı: `218a46f` — test artık
`closes >= 1 && conns == 0`'ı birlikte bekliyor.)*

**Doğrulama.** fmt temiz · clippy `-D warnings` 0 uyarı · test **411**
geçti / 0 hata / 1 ignored (409 + 2 katman testi). Loadgen (50 istemci,
412d863 ↔ HEAD): tcp `left=50 errors=0 server_closes=0`, `snap_total`
4100 ↔ 4150, `out_bps_per_conn` 11021 ↔ 10961, `step_p50_fine_us` 32 ↔
40; udp 4150 ↔ 4150, 11019 ↔ 11053, 40 ↔ 32; sharded N=4 (8 sn) 11526 ↔
11470, 7129 ↔ 7093, 24 ↔ 40 — dönüşümlü üçer A/B'de taban 32/32/24,
HEAD 40/24/16: gürültü içinde.

## Ölçüm kaydı (sharded yeniden ölçüm + write-stall A/B)

Kod değişikliği yok; iki ölçüm ve bir doküman düzeltmesi.

**1. Sharded kapasite ölçümü yeniden alındı.** Fold denetimi turu,
loadgen'in shard raporlarını katlarken shard 0'ı her SUM'da iki kez
saydığını buldu (50 istemcili 4-shard koşusunda `joins=61`, `groups=5`).
"Oda segmentasyonu turu"nun 10k tablosu bu katlamadan geçmişti, o yüzden
sonuç bugünkü kodla yeniden alındı (`--orchestrate 10000 --procs 8 --pin
--duration 30`, `all`; write-stall koruması ölçüm için kapalı — açıkken
sunucu doymuş istemcileri keser, aşağıya bkz.):

| Konfig | adım p50 | adım max | bütçe aşımı | (orijinal aşım) |
|---|---|---|---|---|
| C1 — tek oda | 6250 µs | 76 483 µs | %2,6 | %3,4 |
| `sharded N=4` | 3126 µs | 25 796 µs | %0,0 | %0,2 |
| `sharded N=8` | 1563 µs | 25 238 µs | %0,0 | %0,0 |

**Sonuç korunuyor:** tek oda 10k'da bütçeyi aşıyor, sharded aşmıyor, N
arttıkça p50 düşüyor. Fold hatası bu sonucu üretmemişti — C1 tek satır
olduğu için katlamadan hiç geçmiyordu. Mutlak sayılar orijinalle birebir
kıyaslanamaz: üç koşuda da loadgen istemci tarafı doyuyor (joined
5934–7032, orijinalde 10 000) ve makinede arka plan yükü vardı; kıyas
sıralama olarak geçerli, mutlak değer olarak değil.

**2. Write-stall kesimlerinin sebebi — A/B.** İlk yeniden ölçümde (koruma
varsayılan açık) C1 koşusunda sunucu 4486 oturumu `write stall` ile kesti,
ama `RESULT` satırı `errors=0` diyordu. Stall gözlemlenebilirliği turu
(aşağıda) sebep-bazlı sayaçları ekledi ve saati frame yerine bayt
ilerlemesine bağladı. Aynı 10k C1 koşusu, iki commit'te dönüşümlü ikişer
kez:

| Koşu | commit | joined | write_stall | outbound_dead | bütçe aşımı |
|---|---|---|---|---|---|
| A#1 | `c2b7160` (yalnız sayaçlar) | 9389 | 4658 | 172 | %2,3 |
| B#1 | `d8d9030` (bayt ilerlemesi) | 9350 | 4202 | 167 | %2,8 |
| A#2 | `c2b7160` | 9568 | 4469 | 67 | %1,6 |
| B#2 | `d8d9030` | 9289 | 4518 | 86 | %3,3 |

Fark gürültü içinde. **Kesilenler yavaş-ama-okuyan istemciler değil,
10 sn boyunca hiç okumayan istemciler** — doymuş loadgen süreçleri
(bağlantı başına 1,2–1,6 MB/sn, ~80 KB frame). Koruma tasarlandığı gibi
çalışıyor; bayt-granüler düzeltme ilke olarak doğru (yavaş okuyucu testle
kilitli) ama bu kesimlerin sebebi değildi. Kök sebep koruma değil, o
senaryonun bant genişliği: 10k oyuncuyu tek odada `all` görünürlükle
sürmek desteklenen bir kullanım değil (AOI / delta / sharding bunun
için var). Kapasite ölçümlerinde artık `--write-stall-secs 0`
kullanılmalı ve `RESULT` satırındaki `server_closes=` her koşuda
okunmalı.

**Açık kalan küçük kusur:** `outbound_dead` (67–172 / koşu) büyük olasılıkla
da write-stall'dır: pump kararını aktörün mailbox'ına `try_send` ile
bırakıyor; aşırı yükte mailbox doluysa karar düşüyor ve aktör kapanışı
sebepsiz `outbound_dead` olarak kaydediyor. Toplam kesim sayısı doğru,
yalnız sebep ataması aşırı yükte kayıyor.

**3. Doküman düzeltmesi.** `CROSS-SHARD.md` §7 hâlâ "delta KABUL
EDİLDİ" ve "always-full … elenen alternatif" diyordu; oysa aynı gün
gelen Faz C (`f431296`) süreç-içi link'i sabit `AlwaysFull` yaptı.
Delta border kodu main'de ama hiçbir çalışan konfigürasyon kullanmıyor
(yalnız testler `force_exchange_modes` ile). §7'ye "Güncel durum" notu
ve §9'a "uykuda" durumu eklendi; tarihî karar metni korunarak.

## Kapatılanlar (stall gözlemlenebilirliği + bayt-granüler ilerleme turu)

**Tetikleyici (ölçüm).** 10k kapasite ölçümü yeniden koşuldu
(`gsb-loadgen --orchestrate 10000 --procs 8 --pin --duration 30`, tek
oda, `all` görünürlük). Sunucu **4486 oturumu** `write stall: nothing
written to the socket for 10s` ile öldürdü — ama loadgen'in `RESULT`
satırı `errors=0` diyordu; öldürmeler ancak log grep'iyle bulundu. O
senaryoda kareler ~80 KB (`max_payload_b=81696`), bağlantı başına çıkış
~1,2-1,6 MB/s, istemci süreçleri doymuş. Koddan doğrulanan iki kusur:

- **Kusur A — öldürmeler görünmezdi.** Sunucunun başlattığı kapanışlar
  için hiçbir sayaç yoktu, sebep bazında hiç yoktu. Tıkanmış bir soket
  ERROR bildirimini taşıyamaz, yani hiçbir istemci-tarafı sayaç
  kıpırdamaz: bir kapasite ölçümü istemcilerinin yarısını sessizce
  dökebiliyordu.
- **Kusur B — stall saati KAREyi ölçüyordu, baytı değil.** Saat yalnız
  `sink.send(frame)` (ve `flush`) BÜTÜN OLARAK tamamlandığında
  sıfırlanıyordu; `FrameWriter::drain` tampon bitene dek `poll_write`
  döngüler. Yani düzenli ama yavaş okuyan istemci (80 KB karelerle
  ~8 KB/s altı) baytlar akarken öldürülüyordu. Log mesajı ("nothing
  written") ve korumanın tasarım ilkesi ("ilerleme, yaş değil") bayt
  diyordu; uygulama kare diyordu. Kareler büyüdükçe koruma sessizce bir
  YAŞ sınırına dönüşüyordu.

4486'nın kaçının yavaş-ama-okuyan (B), kaçının hiç okumayan olduğu
bilinmiyor. Kommit sırası bu yüzden: **önce gözlemlenebilirlik
(davranış değişmez), sonra davranış düzeltmesi** — ebeveyn 10k ölçümünü
iki kommitte A/B olarak koşacak.

| Kommit | Konu | Test |
|---|---|---|
| `c2b7160` | Sunucu kapanışlarını sebep bazında say (+ `--write-stall-secs`) | 388 → 404 |
| `debdac9` | Stall saatini bayt-granüler yap | 404 → 408 |
| `edb5baf` | WS yazıcı kuyruğunun uyandırmasını kayıtlı tut (yan bulgu) | 408 → **409** |

Hiçbir test silinmedi, gevşetilmedi, `#[ignore]` eklenmedi. Wire
protokolü değişmedi; loadgen'in iç metrik formatı GSM7 → GSM8.
Mutex/RwLock/parking_lot/select! yok; clippy 0 uyarı.

### 1. `c2b7160` — sunucu kapanışları, sebep bazında

**Taksonomi** (`gsb_core::conn::ServerClose`; dışa açım sırası
`ServerClose::ALL`, yeni sebep SONA eklenir):

| Sebep (`reason=`) | Nerede karar veriliyor |
|---|---|
| `idle_timeout` | reader pump'un idle penceresi (TCP/TLS/WS/QUIC) **ve** rUDP demux'un idle süpürmesi — soketsiz taşımadaki aynı koruma |
| `write_stall` | writer pump'un ilerleme saati |
| `rel_dead` | rUDP REL bandı: ACK ilerlemesi yok **veya** retransmit birikimi tavanı aşıldı (`UdpWriter::die`) |
| `violation_budget` | bağlantı aktörünün ağırlıklı ihlal bütçesi tükendi |
| `preauth_budget` | SECURITY §3.3 pre-auth kare bütçesi aşıldı |
| `stream_rejected` | taşıma gelen bayt akışını REDDETTİ — reader pump'un `InvalidData` çıkışı: `max_frame_bytes` üstü kare, çözülemeyen kare gövdesi, WS protokol ihlali, bozuk TLS kaydı |
| `conn_cap` | doğumda red: `max_connections` |
| `unauth_cap` | doğumda red: `max_unauth_conns` (§4) |
| `superseded` | aynı kimlik aynı odada yeni oturum açtı ("en son kazanan") |
| `room_gone` | oturumun odası yok edildi / öldü (`ConnIn::RoomGone`) |
| `outbound_dead` | giden kanal KAPALI bulundu (`w_closing`) ve kayıtlı bir hüküm yok |

**Bilerek sayılmayanlar.** İstemci-tarafı son (EOF, RST, WS kapanış
el sıkışması, TLS `close_notify`) — aile "sunucu çalışırken hangi
oturumları döktü" sorusunu yanıtlar, istemcinin gitmesi dökme değildir;
bu ayrım koda da yazıldı (`ConnIn::Closed` hüküm taşımaz). **Sunucu
kapanışı** (`ConnIn::Shutdown`): oturum hakkında bir hüküm değil, ve
zaten gözlenemez — toplayıcı ticker'la aynı teardown'da çıkar, bu
kapanışlar raporlanır ya da raporlanmaz, zamanlamaya göre (rUDP'de
FIN olmadığı için her koşunun sonunda 50 oturum böyle kapanır —
sayılsaydı her temiz rUDP koşusu `server_closes=50` derdi). **Ticket
/ protokol sürümü reddi**: bağlantı açık kalır (ERROR 10 / 13); yalnız
SELİ kapatır, o da `violation_budget`'tır. **Girdi-boşta tavanı**
(`max_idle_input_secs`): ENTITY'yi disconnect politikasına verir,
taşıma oturumunu bitirmez.

**Yol.** `ConnIn::ServerClosed` artık `cause: ServerClose` taşır (her
gönderici — iki pump, rUDP demux ve writer, registry'nin üç yolu —
sebebini koyar); yeni `ConnIn::StreamRejected`, `Closed`'ın sessiz
teardown ikizidir (önceden bu çıkış "peer closed" gibi `Closed` olarak
geliyordu — **istemci kapanışı sanılan bir sunucu kararıydı**). Bağlantı
aktörü İLK hükmü kaydeder (bildirim sonra gönderilemezse `w_closing`
hükmün üstüne yazmaz) ve tek çıkışındaki SON `ConnSample`'da bir kez
raporlar (`server_close: Option<ServerClose>`; hüküm varsa tüm
deltalar sıfırken bile son örnek gider — doğumda reddedilen bağlantı
hiç kare görmemiştir). Toplayıcı `NetReport::server_closes`'a
(`ServerCloses`, sebep başına kümülatif) SUM'lar. Dışa açım:

- Prometheus: **tek aile, `reason` etiketi** —
  `gsb_net_server_closes_total{reason="write_stall"}`; bilinen her
  sebep sıfırken de basılır (ilk artıştan itibaren rate/alert
  alınabilsin).
- Log satırı (`scope=net`): `server_closes=<toplam>` + sebep başına
  sabit anahtar `server_close_<reason>=N`.
- loadgen: GSM8 (net kapsamında `violations`'dan sonra sebep başına
  bir `u64`), `RESULT` satırında `server_closes=<toplam>` (tek sayıya
  grep bununla çalışır) + her sebep için `server_close_<reason>=N`
  (hepsi her zaman mevcut, `req_rej_*` ailesi gibi). İnsan özeti
  `errors=` yanına `server_closes=` koyar, `server closes: total=..
  by_reason=..` satırı basar, ve toplam sıfırdan büyükse `WARNING: the
  server ended N session(s) on its own initiative (..); errors=0 counts
  only what the clients observed — this run is NOT clean`.

**Ölü giden yolun atfı.** Writer pump çıkınca giden kanal kapanır;
aktör bunu önce başarısız bir `send` olarak öğrenir — tipik olarak
tıkanmış pump'ın boşaltmayı bıraktığı o DOLU kanalda park etmişken.
Pump'ın kendi raporu (`ServerClosed`, ya da reader'ın `Closed`'ı) o anda
mailbox'ta ARKADA bekliyordur; yalnız gönderim hatasına bakan bir aktör
her write stall'ı `outbound_dead` diye yazardı. İki parça:

1. Writer pump stall hükmünü artık giden kanalı kapatmadan ÖNCE
   `try_send` ile postalar (park edemez); yalnız DOLU mailbox postayı
   kapanıştan sonraki awaited `send`'e erteler (eski sıra).
2. Aktör `w_closing`'de çıkmadan önce mailbox'ını senkron `try_recv`
   ile tarar (`adopt_pending_close`; await eklenmez, döngü zaten
   çıkıyor): bekleyen sunucu hükmü benimsenir, bekleyen peer kapanışı
   → sayılmaz, bekleyen shutdown → sayılmaz, hiçbiri yoksa
   `outbound_dead`.

**Elenen alternatifler.**

- **Registry'de saymak** (`RegistryMsg::ConnClosed`'a sebep eklemek).
  Lehine: registry'ye gönderim awaited `send`, kayıpsız; sayaç
  kümülatif örnekte, düşen örnek zararsız. Elenme: doğumda reddedilen
  bağlantılar registry tablosuna HİÇ girmez (red yolu tam da budur),
  yani `conn_cap`/`unauth_cap` ayrı bir yan yol isterdi; ve hüküm
  aktörde oluşuyor. Seçilen mevcut `ConnSample` yolu; **kabul edilen
  bedel** (kodda belgeli): son örnek de `try_send`, kanal kapanış anında
  DOLUysa hüküm kaybolur ve kayıp hiçbir yerde sayılmaz (aktör gitmiştir).
  Kanal 4096 derin ve her tick boşaltılıyor; bir tick içinde binlerce
  kapanış gerekir.
- **Reason string'ini ayrıştırmak** (`"write stall: …"`, `"idle
  timeout: …"`). Elenme: mesaj metni insan için; sayaç tipe bağlı olmalı.
  Yeni bir varyant `ServerClose::index`/`label`'in tam `match`'inde
  derlenmez.
- **Sebep başına ayrı Prometheus metriği**
  (`gsb_net_server_closes_write_stall_total`). Elenme: DESIGN'ın
  "id/kategori isimde değil etikette" kuralı; tüketici aileyi toplar ya
  da filtreler.
- **Shutdown'ı saymak.** Elenme: yukarıda — hüküm değil, gözlenemez,
  rUDP'nin her temiz koşusunu kirletirdi.
- **Stream reddine ERROR 9 bildirimi eklemek.** Bu kommitte davranış
  değişmez kuralı gereği ELENDİ (açık kalan küçük iyileştirme; §7).
- **`outbound_dead`'i doğrudan `write_stall` saymak.** Elenme: writer
  pump bir yazma HATASINDA da çıkar (peer gitti — istemci-tarafı son);
  ikisini ayıran tek şey mailbox'taki rapor.

**`--write-stall-secs F`** (loadgen): `--idle-timeout-secs`'in birebir
aynası (0 = kapalı, belirtilmezse sunucu varsayılanı 10); in-process
sunucuya, `--serve`'e ve `--orchestrate`'in sunucu sürecine taşınır
(orkestrasyonda sunucu komut satırında `--write-stall-secs 7.5`
görüldü). `--help` güncellendi.

### 2. `debdac9` — stall saati bayt sayar

Saat artık taşımanın kabul ettiği HER baytta sıfırlanır:

- **`gsb_net::pump::WriteProgress`** — yazıcı üzerinde monoton bir bayt
  sayacı; `spawn_pumps` bunu şart koşar. `FrameWriter` (TCP, TLS, QUIC)
  `n > 0` dönen her `poll_write`'ta artırır. WS kapısının soketini
  kendi görevi yazar; sayacı o görevin tek yazar olduğu bir atomik
  (`Arc<AtomicU64>`; kapı zaten `closing: Arc<AtomicBool>` paylaşıyordu)
  ve görev `write_all` yerine bir `write` döngüsüyle yazar (tek
  `write_all` future'ı yalnız KARENİN TAMAMI çıkınca "bitti" derdi).
  `spawn_socket_writer` görevin ve pump'ın sayacını TEK yerden, aynı
  `Arc` olarak verir — pump'ın yazıcısı hiçbir şeyin yazmadığı bir
  sayaca bağlanamaz.
- **Sayaç pump'a nasıl ulaşıyor** (kilitsiz): pump yazıcının sahibi ve
  bekleyen bir `send` boyunca ona `&mut` ile erişir; `SinkExt::send`'in
  future'ı o TEK `&mut`'u tuttuğu için bekleme sırasında kimse sayaca
  bakamazdı. Onun yerine `writer::op::Op`: aynı üç adımı yapar
  (`poll_ready` → `start_send` → `poll_flush`), aynı `&mut`'u tutar,
  sayacı O ödünç üzerinden okur — her poll'da (pump soket yazılabilir
  olduğunda, yani tam baytlar akarken uyandırılır) ve her deadline'da.
  Deadline dolduğunda baytlar ilerlemişse pencere yeniden başlar ve
  AYNI bekleyen işlem yeniden beklenir: timeout'a `&mut op` verilir,
  `op` taşınmaz — yarım yazılmış kare ne kaybolur ne çift gönderilir.
  Görev yok, zamanlayıcı yok, kilit yok; tek-await + deadline idiomu
  aynen duruyor.

**Önerilen şekilden sapma ve gerekçesi.** Önerilen: sayaç yalnız
deadline'da kontrol edilir, ilerlediyse pencere yeniden başlar. Somut
sorun: kare ORTASINDA okumayı bırakan peer, son bayttan sonra bir
pencere değil **iki pencereye kadar** yaşar (pencere son kontrolden
başlar, son bayttan değil) — belgelenen 10 sn'lik sınır sessizce 20
sn olurdu. Sapma küçük: sayaç ayrıca her poll'da okunur ve değiştiği
AN damgalanır; soketi pump'ın sahip olduğu kapılarda (TCP/TLS/QUIC)
sınır yeniden son bayttan itibaren bir penceredir. WS kapısında soket
başka görevde yazıldığı için sayaç ilk kez deadline'da görülür, orada
sınır iki pencereye kadardır (belgelendi).

**Kalıntılar (belgelendi).** (1) TLS: kare tamponu boşaldıktan sonra
rustls'in hâlâ tuttuğu son ≤64 KiB `poll_flush` içinde boşalır ve bayt
sayısı raporlanmaz; pencere başına bundan yavaş okuyan peer bir karenin
KUYRUĞUNDA hâlâ takılabilir. (2) Çekirdek: Linux, dolu bir gönderim
tamponunda bloklu yazıcıyı ancak tamponun yaklaşık üçte biri
boşalınca uyandırır; tamponu B bayt olan bir soket için B / (3 ×
pencere)'den yavaş okuyan istemci uyanmalar arasında hâlâ sessiz
görünür. Bu uygulama tarafından ölçmenin doğal sınırı — ve 10k
senaryosunda neden önemli olduğunu da açıklar: bellek baskısında
çekirdek tamponları küçülür, 80 KB'lık kare bir uyanmada sığmaz, kare
saati çok-uyanmalı her kareyi "ilerleme yok" sayardı.

**Elenen alternatifler.**

- **Yalnız deadline'da gözlem** (önerilen şekil): yukarıda — iki kat
  sınır.
- **Yazıcıda zaman damgası** (`poll_write` başına `Instant::now()`, WS
  için atomik nanosaniye). Elenme: poll-anı gözlemi aynı hassasiyeti
  saf bir sayaçla veriyor; WS'de de damgayı taşımak için ikinci bir
  atomik alan gerekirdi.
- **Deadline'da işlemi düşürüp yeniden kurmak.** Elenme: yarım yazılmış
  kareyi kaybeder ya da çift gönderir.
- **Her kapıda `Arc<AtomicU64>`.** Elenme: TCP/TLS/QUIC'te sayaç zaten
  pump'ın kendi görevinde; paylaşım yalnız soketi başka görevde olan
  WS'de gerekli.
- **`Sink` trait object üzerinden okumak.** Mümkün değil: pump yazıcı
  tipinde generic, ama bekleyen `send` `&mut`'u tuttuğu sürece hiçbir
  trait metodu çağrılamaz — sorun tip değil, ödünç.

### 3. `edb5baf` — yan bulgu: WS kuyruğu pump'ı uyandırmıyordu

WS için bayt testini yazarken çıktı (koddan ve testle doğrulandı).
`WsWriter::poll_ready` her poll'da taze bir `reserve_owned` future'ı
kuruyor, `Pending` dönünce DÜŞÜRÜYORDU. Bekleyen bir reserve'ü düşürmek
bekleyiciyi kanalın bekleme listesinden SİLER: sonradan boşalan slot
kimseyi uyandırmaz. Kapının 64 karelik kuyruğu bir kez dolunca writer
pump, başka bir şey onu poll edene dek uyudu — stall saati kapalıyken
SONSUZA dek (oturum bir daha bayt almaz), açıkken yalnız saatin
deadline'ı uyandırıyordu: kare saati altında bu, kuyruğu bir pencere
boyunca dolu kalan HER WS oturumunu, soket ne kadar hızlı boşalırsa
boşalsın öldürüyordu. Düzeltme: rezervasyon artık
`tokio_util::sync::PollSender` (bekleyen reserve future'ını poll'lar
arasında saklar). Kilit: `ws::tests::queue` — stall saati hiç yokken,
her şey tıkanana kadar hiç okumayan sonra hızla okuyan peer, 100 × 256
KiB'lik karenin her baytını almalı; düzeltmeden önce ~75 karede takılır.

### 4. Test tekniği

- **TCP** (`tcp::tests::slow_reader`): iki çekirdek tamponu da küçültüldü
  (`SO_SNDBUF`/`SO_RCVBUF` = 4096; açık boyut otomatik ayarı da
  kapatır), peer 8 ms'de bir 1 KiB okur, TEK 256 KiB'lik kare ~2 sn'de
  (~7 pencere) boşalır; peer kareyi eksiksiz almalı, hiçbir kapanış
  raporlanmamalı, ve boşalma ≥ 3 pencere sürmeli (tamponlar kareyi
  yutarsa test boş geçmesin diye).
- **QUIC** (`quic::tests::slow_reader`): geçen turun tekniği — alım
  penceresini İSTEMCİ ayarlar (2 KiB), sunucu okunanın en fazla o kadar
  önüne geçebilir, çekirdek tamponu yok. 10 ms'de 1 KiB, 128 KiB kare ~5
  pencere.
- **WS** (`ws::tests::slow_reader`): gerçek kapıdan yapılamadı — kabul
  edilen soketin tamponu testten küçültülemiyor, loopback'te gönderim
  tamponu 4 MiB'a kadar otomatik büyüyor ve çekirdek uyanma histerezi
  yavaş okuyucuyu saniyeler arayla megabaytlık patlamalara çeviriyor
  (bu çekirdeğe dair bir ifade, saate değil). Onun yerine kapının kendi
  soket-yazıcı görevi ve pump-yüzlü yazıcısı, taşımanın kullandığı AYNI
  `spawn_socket_writer`'la, dar bir soket üzerinde kuruldu; pump dolu
  kuyrukta beklerken peer ~125 KB/s okur. Gerçek kapıdan: hiç okumayan
  WS peer'ı hâlâ ölür (`a_deaf_ws_peer_still_dies`).
- TCP ve QUIC testleri `c2b7160` üzerinde KIRMIZI (TCP: 40 KB okunmuşken
  374 ms'de `WriteStall`; QUIC: 29 KB'ta akış kapandı). WS testi yeni
  API'yi (`spawn_socket_writer`) kullandığı için eski kommitte derlenmez;
  onun kilidi mutasyonla doğrulandı (§5). Mevcut sağır-peer testleri
  (`tcp::tests::stall`, `write_stall.rs`) yeşil kaldı.

### 5. Mutation-check

| # | Bozma | Düşen testler |
|---|---|---|
| 1 | pump idle hükmü `IdleTimeout` → `WriteStall` | `server_closes::a_silent_client…`, `tcp::tests::idle` (2), `ws::…::idle_timeout_still_applies…` |
| 2 | writer hükmü `WriteStall` → `IdleTimeout` | `write_stall.rs`, `tcp::tests::stall` (2) |
| 3 | `ViolationBudget` → `PreauthBudget` | `closes::the_violation_budget…`, `server_closes::a_violating_client…` |
| 4 | `PreauthBudget` → `ViolationBudget` | `closes::the_preauth_budget…` |
| 5 | `adopt_pending_close` mailbox'a bakmıyor | `closes::a_dead_outbound_path_adopts…`, `…_behind_a_peer_close…` |
| 6 | writer önce kanalı kapatıp SONRA postalıyor | `the_stall_verdict_is_posted_before…` — 10 koşunun 10'unda (yarış zorla kaybettirilemez; ölçülen oran) |
| 7 | reader'ın `InvalidData` → `StreamRejected` sınıflaması kaldırıldı | `an_oversized_frame_is_a_stream_rejection…`, `a_protocol_violation_is_a_stream_rejection` |
| 8 | toplayıcı hükmü eklemiyor | `metrics::tests::closes`, `server_closes` (2) |
| 9 | `Op::observe` pencereyi yeniden başlatmıyor (= kare saati) | TCP, QUIC, WS yavaş-okuyucu testleri (3) |
| 10 | `FrameWriter` bayt saymıyor | TCP, QUIC yavaş-okuyucu |
| 11 | WS soket görevi bayt saymıyor | WS yavaş-okuyucu |
| 12 | `WsWriter::poll_ready` `Pending`'de rezervasyonu bırakıyor (`abort_send`) | `ws::tests::queue` |

Kilitlenmeyen tek şey: poll-anı gözleminin HASSASİYETİ (deadline-only
gözleme geri dönmek yavaş-okuyucu testlerini geçirir; farkı "son
bayttan bir pencere" ile "iki pencereye kadar" arasında ölçen bir
zamanlama testi kırılgan olurdu).

### 6. Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **409 passed / 0 failed / 1 ignored**
- `gsb-loadgen -- 50 --duration 3` → `left=50 errors=0
  server_closes=0`, 11 sebep anahtarının hepsi 0; `--transport udp` ile
  aynı (registry `conns=50 closes=0` — rUDP'de FIN yok, oturumlar
  shutdown'la biter ve bilerek sayılmaz); `--topology sharded
  --shard-count 4 --duration 8` → aynı. Panik yok.
- Negatif gösterim: `gsb-loadgen 5 --duration 2 --idle-timeout-secs
  0.05 --move-ms 500` → `errors=0 server_closes=5`, `by_reason=
  idle_timeout:5` ve WARNING satırı.
- 10k A/B ölçümü BU TURDA KOŞULMADI (ebeveyn `c2b7160` ve `debdac9`
  üzerinde koşacak).

### 7. Yapılmayanlar / açık kalanlar

- `stream_rejected` kapanışlarına ERROR 9 bildirimi (davranış değişikliği;
  istemciye "karen reddedildi" demek faydalı olurdu).
- Son `ConnSample`'ın dolu kanalda kaybı sayılmıyor (§1, kabul edilen
  bedel).
- TLS'in ≤64 KiB kuyruk kalıntısı ve çekirdek uyanma histerezi (§2).
- loadgen istemcisi ERROR 9'u hâlâ yalnız `violation` / diğer (=
  `cap_rejected`) diye ayırıyor; sunucu tarafı sayaç artık otorite.

## Kapatılanlar (sayaç envanteri kapanış turu)

**Tur kapsamı.** P0'ın "Kalan metrik sayaçları için doğru-yol testleri"
maddesi kapandı, ve yanında son turun geçerken fark ettiği iki öksüz
alan (`shipped_frames` / `private_frames`) bir tüketiciye bağlandı.

Turun ilk işi ROADMAP'teki listeye GÜVENMEMEK oldu: liste bayattı.
"reject-bucket wiring + sayaç envanteri turu"nda yazıldığından beri üç
tur (minimum sayaçlar, park sızıntısı + shard metrik boşluğu, metrik
fold denetimi) adı geçen alanların bir kısmını zaten kapatmıştı.
Envanter sıfırdan yeniden türetildi — metrik yüzeyine ulaşan her tip
(`RoomSample`, `RegistrySample`, `ConnSample`, `NetReport`,
`UdpClientStats`) alan alan tarandı, her alan için "gerçek üretim
yolunu süren ve O alanın arttığını assert eden bir test var mı?"
sorusu koddan yanıtlandı. Tablo aşağıda (§2), turdan ÖNCE ve SONRA.

Kapsama kriteri değişmedi (reject-bucket turunun koyduğu standart):
elle kurulmuş bir örnek üzerinde assert etmek saymaz, gerçek aktörü
sürmek gerekir; ve bir alanın YALNIZCA sıfır olduğunu assert eden bir
test, hiç yazılmayan bir alandan ayırt edilemez — bu turda
`requests_timed_out` tam olarak o durumdaydı.

Test sayısı 361 → **388** (+27; hiçbir test silinmedi, gevşetilmedi,
`#[ignore]` eklenmedi). Wire protokolü değişmedi; loadgen'in İÇ metrik
export formatı GSM6 → GSM7 (iki uç da aynı ikili). Mutex/RwLock/
parking_lot/select! yok; clippy 0 uyarı.

### 1. Yöntem: her test mutation-check'li

Turun her testi kırılarak doğrulandı: sayacın artışı bozuldu, testin
DÜŞTÜĞÜ görüldü, bozma geri alındı. Toplam **46 mutasyon**, hepsi
öldürüldü; kırılamayan test olmadı. Bozmalar üç sınıfta toplandı:

1. **hiç artmayan sayaç** (`+= 1` → `+= 0`) — "her zaman sıfır" hatası,
   bu depoda iki kez gerçekten olmuş bir hata;
2. **yanlış kola bağlı sayaç** — `leaves`'i join yoluna, `joins`'i
   conn-open yoluna, `violations`'ı gelen-kare yoluna, `rooms_died`'ı
   sıradan destroy'a bağlamak. Bunlar rapor satırında YAN YANA duran
   alanlar; yanlış kova operatöre kendinden emin ve yanlış cevap verir;
3. **doğru sayaç, yanlış semantik** — `snap_bytes_max`'ı koşulsuz
   atamak (tepe yerine sonuncu), `lagged_ticks`'e `missed` yerine 1
   eklemek, ince histogramı sabit bir sayıyla binlemek, `gave_up`'a
   kuyruk boyu yerine 1 eklemek, `oob_dropped`'ı pencere sınırından bir
   erken ateşlemek.

Üçüncü sınıf yeni testlerin çoğunun şeklini belirledi: her test, sayacı
KENDİSİNE en çok benzeyen komşusundan ayıran bir gözlem içeriyor
(detay §3).

### 2. Envanter (ÖNCE → SONRA)

**evet** = gerçek yolu tetikleyen ve o alanın arttığını assert eden test
var; **kısmi** = yalnız bir aktörde (oda/shard) var; **smoke** = sayaç
okunuyor ama yalnız sıfır/salimlik; **hayır** = yok; **N/A** = ölçüm
değil (kimlik, zaman damgası, konfigürasyon yankısı).

**RoomSample** (iki aktör de üretir: oda ve shard):

| Alan | Önce | Sonra | Test |
|---|---|---|---|
| `room`, `emit_at`, `budget_us` | N/A | N/A | kimlik / damga / konfig yankısı |
| `steps` | evet | evet | `room_counters_flow_to_collector` |
| `lagged_events`, `lagged_ticks` | **hayır** | **evet** | `lag_counts_one_event_and_every_missed_tick_index` |
| `step_min/max/sum_us`, `step_hist` | evet | evet | `duration_counters_stay_mutually_consistent` + unit |
| `step_fine_hist` | **kısmi** (shard) | **evet** | `the_rooms_step_fills_the_fine_duration_histogram` |
| `late_min/max/sum_us` | evet | evet | `a_faster_tick_lowers_the_rooms_late_minimum` |
| `dropped_frames` | evet | evet | `room_counters_flow_to_collector` |
| `keepalive_resends` | **kısmi** (shard) | **evet** | `keepalive_resend_counts_the_unchanged_group_only` |
| `snapshots` | evet | evet | flow + `snapshot_bytes_sum_and_peak_…` |
| `snap_bytes`, `snap_bytes_max` | **hayır** | **evet** | `snapshot_bytes_sum_and_peak_track_the_encoded_payloads` |
| `snap_overflows` | **hayır** | **evet** | `every_oversized_snapshot_is_counted_not_just_the_first` + `the_overflow_boundary_is_strictly_above_the_budget` |
| `snap_records` | **hayır** | **evet** | `snap_records_accumulates_the_logics_encoded_record_count` |
| `shipped_bytes` | **hayır** | **evet** | `shipped_counters_count_every_fanout_copy_…` |
| `shipped_frames`, `private_frames` | **hayır + ÖKSÜZ** | **evet + taşınıyor** | aynı + §4 |
| `joins` | evet | evet | flow test |
| `leaves` | **hayır** | **evet** | `leaves_counts_the_control_plane_leave` |
| `detached`, `resumes`, `resume_rejected_stale`, `detach_expired_*` | evet | evet | `tests/reconnect.rs` |
| `requests_local`, `requests_external` | **kısmi** (shard) | **evet** | `requests_local_counts_every_same_tick_answer_not_every_tick`, `requests_external_counts_the_delegation_…` |
| `requests_rejected_*` (6) | evet | evet | `buckets::reject_bucket_*` |
| `requests_timed_out` | **hayır** (yalnız `== 0`) | **evet** | `requests_timed_out_counts_the_sweep` |
| `requests_late` | evet | evet | `reconnect.rs`, `rpc_shard.rs` |
| `pending_requests` | **kısmi** (shard) | **evet** | `requests_external_counts_the_delegation_and_pending_tracks_flight` |
| `groups`, `members`, `max_group` | evet | evet | flow test |
| `metrics_dropped` | **smoke** | **evet** | `a_full_metrics_channel_drops_the_sample_and_counts_it` |

**RegistrySample** — turdan önce **tek bir alanı bile** test edilmemişti
(`conns` hariç, o da park sızıntısı turunda dolaylı):

| Alan | Önce | Sonra | Test |
|---|---|---|---|
| `rooms`, `rooms_created`, `rooms_destroyed` | hayır | **evet** | `room_creates_and_destroys_are_flow_while_rooms_is_the_table_size`, `a_destroy_of_an_absent_room_is_not_counted` |
| `rooms_died` | hayır | **evet** | `an_unexpected_death_counts_in_rooms_died_not_rooms_destroyed` |
| `conns`, `opens`, `closes` | smoke | **evet** | `open_and_close_counters_are_flow_while_conns_is_the_table_size` |
| `joins`, `leaves` | hayır | **evet** | `join_and_leave_counters_are_independent_of_open_and_close` |
| `metrics_dropped` | hayır | hayır | **açık kaldı** — §5 |

**ConnSample / NetReport:**

| Alan | Önce | Sonra | Test |
|---|---|---|---|
| `actions_dropped` (+ `actions_dropped_top`) | evet | evet | `e2e::flooder_drops_attributed` |
| `bytes_in`, `bytes_out` | smoke | evet (in) / smoke (out) | `violations_frames_and_the_final_flag_reach_the_sample` |
| `frames_in`, `frames_out` | hayır | **evet** | aynı + `ordinary_answered_frames_are_not_violations` |
| `violations` | hayır | **evet** | aynı ikili |
| `last` | hayır | **evet** | `violations_frames_and_the_final_flag_reach_the_sample` |
| `metrics_dropped` | hayır | hayır | **açık kaldı** — §5 |

**UdpClientStats** — dördü de turdan önce testsizdi:

| Alan | Önce | Sonra | Test |
|---|---|---|---|
| `dup_in` | hayır | **evet** | `dup_in_counts_a_server_retransmit_without_redelivering_it` |
| `oob_dropped` | hayır | **evet** | `oob_dropped_counts_only_past_the_reorder_window` |
| `retrans_out` | hayır | **evet** | `retrans_out_counts_re_sends_and_respects_the_rto` |
| `gave_up` | hayır | **evet** | `gave_up_counts_everything_outstanding_when_the_band_dies` + `an_idle_client_never_gives_up` |

### 3. Testlerin şekli: sayacı komşusundan ayırmak

Bir sayacı "arttı mı?" diye sormak yetmiyor; bu depoda bulunan hatalar
sayacın YANLIŞ KOLA bağlı olmasıydı. Bu yüzden her test, alanı ona en
çok benzeyen şeyden ayıran bir gözlem taşıyor:

- **tepe ≠ sonuncu** — `snap_bytes_max` daha KÜÇÜK bir yükten sağ
  çıkmalı. MTU hazırlık sinyali tam olarak "büyük snapshot patlaması +
  sessiz bir tick" durumunda yanlış okurdu;
- **her seferinde ≠ bir kez** — `snap_overflows` her aşan emit'i sayar;
  uyarı grup başına BİR kez düşer, yani oranı ölçebilen tek şey sayaç.
  Grup başına sayan bir sayaç, her tick aşan bir odada 1 okurdu;
- **akış ≠ gauge** — `opens`/`closes` vs `conns`,
  `rooms_created`/`rooms_destroyed` vs `rooms`. "Conn cap neden
  bağlıyor?" sorusunun cevabı closes'ın KAYBOLUP kaybolmadığı, ve bunu
  gauge söyleyemez;
- **ölüm ≠ destroy** — `rooms_died` iki yönden de assert edildi: ölüm
  destroy'a yazılmamalı, sıradan destroy da `rooms_died`'ı kirletmemeli.
  Ölümü destroy sayan bir sunucu, her odası patlarken sağlıklı ama
  yoğun bir kontrol düzlemi gibi görünürdü;
- **ihlal ≠ trafik** — ilk ihlal testinde GELEN her kare zaten bir
  ihlaldi, yani gelen-kare yoluna bağlı bir `violations` o testi
  geçerdi. İkinci test bunu kapatıyor: bir throttle aralığındaki iki
  heartbeat = 2 giren, 1 çıkan ack, 0 ihlal;
- **kadans ≠ kayıp** — `metrics_dropped`, örneklemeyen bir adımı
  saymamalı. Sayarsa varsayılan konfigürasyonlu her oda (30 Hz tick, 1
  Hz kadans) saniyede 29 kayıp bildirir ve sürekli doymuş görünür;
- **bant ölümü ≠ sessizlik** — `gave_up` bekleyen işin tamamını birden
  sayar ve `is_established`'ı düşürür; boşta bir istemci ise ne kadar
  sessiz kalırsa kalsın ölmez (ölüm saati cevapsız İŞİ ölçer).

### 4. İki öksüz alan: `shipped_frames` / `private_frames`

Son turun geçerken not ettiği bulgu doğrulandı: iki sayaç da fan-out
yazıldığından beri İKİ aktör tarafından da tutuluyor, `RoomSample`'a
konuyor ve orada bitiyor. Ne akümülatör, ne renderer, ne Prometheus
yüzeyi, ne loadgen'in codec'i ya da foldu onlara dokunuyordu — yani
hiçbir tüketici okuyamıyordu.

**Karar: TAŞIMAK** (silmek değil). `shipped_bytes` ile gereksiz
değiller:

- `shipped_bytes / shipped_frames` = ortalama kare boyu. Datagram
  taşıması (rUDP, ağaçta ve deneysel) BAYT kadar PAKET ile de sınırlı:
  aynı bayt hızı iki katı karede farklı bir yük demektir, ve bunu bayt
  sayacı tek başına söyleyemez;
- `shipped_frames − private_frames` = fan-out'un yayın yarısı.
  Özel/yayın ayrımı, `snap_records`'un encode tarafından yanıtladığı
  sorunun gönderme tarafındaki karşılığı; encode tarafı kaç KOPYA
  çıktığını bilmiyor.

**Elenen alternatif — kaynakta silmek.** İki aktörden de sayaçları
kaldırmak daha küçük bir yama olurdu ve "kimsenin okuyamadığı sayaç"
kuralını da sağlardı. Elendi çünkü yukarıdaki iki soru gerçek ve başka
hiçbir alan onları yanıtlamıyor; ayrıca rUDP ağaçta duruyor, yani paket
sayısı bugünden ilgili.

Uçtan uca taşındı, her durakta bir kural ve bir tüketici ile:
`RoomReport` alanları → akümülatör doğrudan geçiriyor (kendi oranları
yok, kümülatif sayaçlar) → `fold_rooms` **SUM** kuralı (`shipped_bytes`
ile aynı sınıf: ayrık iş üzerinde kümülatif sayaç; DESIGN §12 tablosunun
SUM satırı da genişletildi) → GSM6 → GSM7 → tüketiciler: `gsb-metric
scope=room` renderer satırı ve loadgen'in `server room (final)` satırı
(türetilmiş `mean_frame_b` ile birlikte).

Foldun tam destructure'ı burada tasarlandığı gibi çalıştı: alanları
eklemek, kural yazılana kadar **E0063 ile derlemeyi kırdı** (§13'ün
derleme-zamanı koruma ailesi).

### 5. Açık kalan (düşük değerli kuyruk)

`RegistrySample::metrics_dropped` ve `ConnSample::metrics_dropped`.
Odanınki bu turda kapandı (`a_full_metrics_channel_drops_the_sample_
and_counts_it`) ve mekanizma üçünde de aynı: dolu bir bounded kanalda
`try_send` başarısız olur, üretici kendi sayacını artırır, sayı bir
sonraki geçen örnekle çıkar. Oda tarafında kapatmak bu davranışı
kilitliyor; kalan ikisi aynı desenin kopyaları ve ikisi de tasarım
gereği zararsız (sayaçlar kümülatif — sonraki örnek her şeyi taşır).
Ayrıca ikisini sürmek için üretici tarafında kanalı doldurmak
gerekiyor: registry olay-tetikli örnekler (bir test durum değişikliği
üretmeli), conn aktörü ise en fazla `METRICS_FLUSH_EVERY`de bir
flush'lar (kısa bir test yalnız son flush'ı görür) — yani her ikisi de
oda sürümünden belirgin biçimde daha kırılgan testler olurdu, aynı
mekanizmayı ikinci ve üçüncü kez doğrulamak için.

`ConnSample::bytes_out` de tam değil: yeni test `bytes_in > 0` assert
ediyor, `bytes_out` ise hâlâ yalnız smoke (loadgen `server_out_bps > 0`).
Kare sayaçları (`frames_out`) tam kapandığı için bayt yarısının
gerileme riski düşük.


## Kapatılanlar (metrik fold denetimi turu)

"minimum sayaçlar turu"nun bıraktığı açık yan bulgu —
`loadgen::report::fold_rooms` minimumlar DIŞINDA da eksik katlıyor —
alan alan değil **toplu** kapandı. Sözleşme buydu: tek tek düzeltmeyi
bırak, foldun tamamını denetle, alan başına bir kural kararlaştır,
kuralı **koda** yaz ve bir sonraki alanın sessizce unutulmasını
YAPISAL olarak imkânsız kıl.

Test sayısı 356 → **361** (+7 yeni fold testi, −2 eski fold testi
yerine geçti; hiçbir test silinmedi, gevşetilmedi, `#[ignore]`
eklenmedi).

### 1. Neden tek tek düzeltmek işe yaramıyordu

Üç tur üç alan düzeltti: `step_min_us`, sonra `late_min_us`, sonra bu
denetim. Sorun alanlar değil **şekildi**. Fold ilk satırın bir
KOPYASINI mutasyona uğratıyordu (`acc = *first`) ve elle seçilmiş bir
alt kümeye dokunuyordu; dokunulmayan alan sessizce shard 0'ın değerini
raporluyordu ve hiçbir şey sormuyordu. Bir alan eklemek derlemeyi
kırmıyordu — kural kod incelemesine bağlıydı, derleme zamanına değil
(§13'ün tam tersi).

### 2. Yapısal koruma

Döngü artık `RoomReport`'u **tam (exhaustive) destructure** ediyor:
`..` yok, atlanan alan yok, kullanılmayan binding yok ("kullanılmayan
binding = kimsenin yazmadığı kural"). `RoomReport`'a alan eklemek
`fold_rooms`'ta **E0027** ile derlemeyi kırıyor — deneyle doğrulandı.
Katlanmayan tek alan (`room`, bir kimlik) gerekçesi yazılmış hâlde
`_`'ye bağlanıyor, yani o da bir karar.

Kural tablosu modülün doc yorumunda, onu uygulayan TEK döngünün
yanında (`crates/gsb-server/src/loadgen/report/fold.rs`); aynı tablo
DESIGN §12'ye de işlendi.

### 3. Merge noktalarının tam listesi (denetimin kapsamı)

| Nokta | Ne birleşiyor | Durum |
|---|---|---|
| `loadgen::report::fold_rooms` | N shard raporu → 1 oda raporu | bu turun konusu; yeniden yazıldı |
| `loadgen::report::report_members` | oda üyeliği, SUM | doğruydu |
| `loadgen::report::report_steps` | rapor tazeliği, MAX | doğruydu |
| `loadgen::report::result::print_report` → `server_hz` | oda başına `hz`'in pozitifleri üzerinden MIN, sonra raporlar arası medyan | doğruydu; fold'un `hz` kuralı buna hizalandı |
| `MetricAccumulator::apply` (`Conn` kolu) | bağlantı aktörlerinin delta örnekleri → net toplamlar, SUM | doğruydu |
| `MetricAccumulator::report` | `net.bytes_out_room` (odaların `shipped_bytes` SUM'u) + üst düzey `metrics_dropped` (oda + registry + conn SUM'u) | doğruydu |
| `metrics::prometheus` (`/metrics`) | **birleştirmiyor** — örnek kimliği başına bir satır (`room="r<id>"`), shard'lar ayrı seri | değişiklik gerekmedi |
| `metrics::render` (log satırları) | **birleştirmiyor** — oda başına bir satır | değişiklik gerekmedi |
| `loadgen::codec` `encode_report`/`decode_report` | birleştirme değil, ama aynı struct üzerinde alan alan geçiş (ayrı-süreç modu) | decoder'ın struct literal'i zaten alan eklemede derlemiyor |

Fold hot path'te DEĞİL: `print_report` koşu sonunda bir kez çalışır;
toplayıcının `report()`'u da oda tick'inin dışındaki toplayıcı
görevindedir (rapor temposu, vars. 1 Hz).

### 4. Değişen alanlar: önce → sonra

Hepsi yalnız **sharded** odayı etkiler; tek odalı rapor hâlâ birebir
kimlik foldudur (erken dönüş).

| Alan | Önce | Sonra |
|---|---|---|
| **her SUM alanı** (`members`, `groups`, `joins`, `leaves`, `snapshots`, `snap_records`, `shipped_bytes`, `dropped`, `lagged_*`, `keepalive_resends`, `snap_overflows`, `resume*`, `detach_expired_*`, iki histogram) | shard 0 İKİ KEZ toplanıyordu (`acc = *first` + tüm dilim üzerinde döngü) | tohum iteratörden tüketiliyor; her shard tam bir kez |
| `late_mean_us` | shard 0'ın ortalaması | shard ortalamalarının **adım-ağırlıklı** ortalaması |
| `dropped_s`, `snap_bytes_s`, `shipped_s` | shard 0'ın oranı | shard oranlarının **SUM**'u (oran ortalanamaz) |
| `requests_*` ailesinin tamamı (10 alan) | shard 0'ınki | SUM |
| `pending_requests` | shard 0'ınki | SUM (gauge, ama **bölünmüş** bir gauge) |
| `metrics_dropped` | shard 0'ınki | SUM |
| `detached` | MAX ("en kötü shard'ın park sayısı") | SUM — `members`/`groups` ile aynı cinsten bölünmüş gauge |
| `budget_us` | shard 0'ınki | MIN (konfigürasyon; aşağıda) |
| `hz` | sıfırları da sayan `min` | rapor **veren** shard'lar üzerinden MIN |
| `step_p50_fine_us` / `step_p90_fine_us` (tüketici) | percentil, katlanmış histograma **`steps`** nüfusuyla soruluyordu | `folded_steps` (histogramın kendi nüfusu) |

Son satır ayrı bir kusurdur ve fold kurallarının etkileşiminden doğar:
`steps` MAX ile katlanır (shard'lar tek global ticker'la aynı adımda),
iki histogram ise SUM ile. Katlamadan sonra ikisi aynı şeyi saymaz; tick
sayısını nüfus diye vermek 4 shard'lık bir odada "p50" etiketi altında
kabaca p12.5'i bastırıyordu.

### 5. Kararların gerekçeleri (elenen alternatifler)

1. **`budget_us` için "shard'lar ayrışırsa bayrak"** — ELENDİ. Yeni bir
   `RoomReport` alanı demekti; codec'e, Prometheus yüzeyine ve log
   renderer'a kadar dalga yapardı, üstelik shard'lar tek `RoomConfig`
   paylaştığı için hiç tetiklenmezdi. Seçilen: **MIN**. Dürüst olan
   taraf budur — `budget_us` aşım oranının ve histogram kenarlarının
   paydası; küçük bütçe aşımı **daha erken** okur (bir eşik için
   güvenli yön). Ayrıca bütçeler ayrışırsa `step_hist` toplamı zaten
   ölçülemez hâle gelir, ki bunu tablo yazılı olarak söylüyor.
2. **`steps` için SUM** (histogram nüfusuyla kendiliğinden uyuşurdu) —
   ELENDİ. `steps` hem `hz` ile karşılaştırılan tick sayısı hem de
   `records_per_tick = Δkayıt / Δadım` paydası; SUM onu shard sayısıyla
   çarpar ve iki okumayı da bozar. Bunun yerine **tüketici** düzeltildi
   (`folded_steps`).
3. **Katlanmış nüfusu `RoomReport`'a yeni bir alan olarak taşımak** —
   ELENDİ: log2 histogramı zaten TAM nüfustur (her adım binlenir, üst
   bin sınırsız → `Σ step_hist == Σ steps`), yani türetilebilen bir
   şeyi tel formatına ve her tüketiciye taşımak olurdu. İnce histogram
   bu işe yaramaz: tavan üstü adımlar kasten onda yok — zaten
   `fine_hist_percentile_us`'in nüfusu parametre olarak almasının sebebi
   bu.
4. **`detached` MAX kalsın** ("en kötü shard'ın park yükü") — ELENDİ.
   Komşuları `members` ve `groups` aynı bölünmüş gauge cinsindendir ve
   SUM'lanıyordu; aynı satırda aynı cinsten iki alanın farklı katlanması
   tam olarak bu turun kapattığı hata sınıfıdır. Ekstremum kalanlar
   (`max_group`, `snap_bytes_max`) bir POPÜLASYON değil, popülasyon
   ÜZERİNDE bir uçtur — tablo bu ayrımı yazıyor.
5. **Fold'u `RoomReport`'a bir `impl` metodu yapmak** — ELENDİ: kural
   loadgen'in tüketici kararıdır (hangi satırın oda, hangisinin shard
   olduğunu bilen taraf), `gsb-core`'un rapor tipinin değil.
6. **Sadece bir doküman tablosu** — ELENDİ (sözleşme zaten reddediyordu):
   bir sonraki alanı unutulmaktan koruyan şey derleyicidir, tablo değil.

### 6. Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **361 passed / 0 failed / 1 ignored**
- `gsb-loadgen -- 50 --duration 3` (tcp ve `--transport udp`) →
  `left=50`, `errors=0`, panik yok.
- **Sharded koşu** (`--topology sharded --shard-count 4`, 50 istemci,
  8 s) — turun etkisinin görünür olduğu yer. `2486dae` tabanı → HEAD:

```text
taban: steps=240 step_p50_fine_us=16 step_p90_fine_us=24 hist=[1200,0,...]
       groups=5 members=61 joins=61 snapshots=1160
       records_per_tick=131.7 overlap_x=2.63
HEAD : steps=240 step_p50_fine_us=32 step_p90_fine_us=40 hist=[960,0,...]
       groups=4 members=50 joins=50 snapshots=928
       records_per_tick=107.7 overlap_x=2.15
```

`hist` toplamı 1200 → 960: 240 adım × 4 shard, taban ise 5 shard'lık
kütle taşıyordu (shard 0 iki kez). `members=61` 50 istemcilik bir koşuda
şeffaf biçimde yanlıştı. `records_per_tick` düşüşü ≈ 4/5 oranındadır,
yani tam olarak fazladan sayılan shard kadar.

### 7. Mutation-check

| Mutasyon | Kırılan |
|---|---|
| `detached` SUM → MAX | 1 test |
| `pending_requests` SUM → MAX | 1 test |
| `dropped_s` SUM → MAX | 1 test |
| `late_mean_us` ağırlıksız | 1 test |
| çifte sayım geri getiriliyor | 3 test |
| `budget_us` MIN → MAX | 1 test |
| `requests_local` katlanmıyor | 1 test |
| `metrics_dropped` katlanmıyor | 1 test |
| `hz` sıfırları da sayıyor | 1 test |
| `fine_percentiles_us` nüfus olarak `steps` alıyor | 1 test |
| `RoomReport`'a alan ekleniyor | **derlemiyor** (E0027) |

Fikstür üç **FARKLI** shard raporu verir ve hiçbir ekstremumu ilk ya da
son shard'a koymaz: aynı girdilerle her yanlış kural doğru görünür.

## Kapatılanlar (AFK sinyali + girdi-boşta tavanı turu)

Teknik borç turunun ürün kararına bıraktığı dört maddeden **üçüncüsü**
kapandı: **AFK / zombi oturum**. Kalan tek madde geçerli girdiye hacim
limiti.

**Sorun (koddan doğrulandı).** Base'in tek canlılık kavramı "herhangi
bir frame geldi" idi (reader pump'un `idle_timeout_secs` penceresi).
Sonsuza dek heartbeat atan ama hiç oynamayan bir istemci, aktif bir
oyuncudan AYIRT EDİLEMİYORDU. Bu canlılık için doğru, AFK için
kullanışsız.

**Karar (parent tasarladı, kullanıcı onayladı): AFK base'in kararı
DEĞİL.** MMO kasabasında kıpırdamadan durmak meşru oynayıştır, MOBA'da
30 saniye hareketsizlik bot devri tetikleyicisidir. Bu yüzden tur iki
parçalıdır: koşulsuz bir **sinyal** ve varsayılan KAPALI bir **tavan**.

Test sayısı 344 → **356** (+12; hiçbir test silinmedi, gevşetilmedi,
`#[ignore]` eklenmedi). Davranış testleri mutation-verified (aşağıda).

### 1. Sinyal — `IdleClock` + `TickCtx::since_input`

**"Aksiyon taşıyan" tanımı YAPISALDIR**, opcode listesi değil — base
oyunun opcode'larını bilmez. Bir oyun yazarının okuyacağı hâliyle:

> Bir kare, bağlantı aktörü onu odaya `Action` olarak ilettiği anda —
> ve tam olarak o anda — girdi saatini sıfırlar (`forward_to_room`):
> mesaj tablosunda KAYITLI her oyun-bandı opcode'u, artı base-bandı RPC
> istek zarfı (`RPC_REQ`). Bağlantı aktörünün kendi yanıtladığı ya da
> reddettiği hiçbir şey saati kıpırdatmaz: AUTH, JOIN/LEAVE, HEARTBEAT,
> bilinmeyen opcode, ihlal bütçesine düşen kare. Nabız *taşımayı* canlı
> tutar (reader'ın idle penceresi), *girdi* saatini olduğu yerde
> bırakır.

Saat READ fazında damgalanır — aksiyonların gerçekten çekildiği yerde.

**Mantık nasıl okur (seçilen dikiş).** `TickCtx` bir `IdleView` alanı
kazandı; `ctx.since_input(player)` her tick hook'unun içinden, await'siz,
bağlantı başına görev açmadan, mantık yüzeyine YENİ BİR METOT EKLEMEDEN
çalışır. `TickCtx` bunun için bir ömür (lifetime) parametresi aldı;
aktörler tick gövdesi boyunca saati kendilerinden DIŞARI taşır (O(1)
pointer takası), çünkü alttaki fazlar `&mut self` alır.

**Elenen alternatifler.**

1. **`TickCtx`'in doğrudan `conns` tablosunu ödünç alması** — ELENDİ.
   Tablo oyunun grup anahtarı `G` üzerinden generic; `TickCtx`'i
   generic yapmak `G`'yi her hook imzasına bulaştırırdı. Ayrıca ödünç
   `self`'ten gelirdi ve `phase_requests`/`broadcast_phase`'in
   `&mut self` çağrılarıyla ödünç çakışırdı (tick gövdesini yeniden
   kurmak gerekirdi).
2. **Mantığın sinyali kendi tutması** (`ingest`'te gelen aksiyonlardan)
   — ELENDİ. Tanımın otoritesi base'dedir; ayrıca her oyun
   join/leave/detach/bot/resume yaşam döngüsünü yeniden yazardı ve
   tavan yine de base'in saatini isterdi.
3. **Mantık yüzeyine yeni bir hook** (`on_idle`, ya da bir
   `idle_since()` sorgu metodu) — ELENDİ: sözleşme "var olan dikişi
   genişlet, yeni hook icat etme"; `TickCtx` zaten her hook'un aldığı
   per-tick dikiştir.
4. **Damgayı `RoomConn` satırında tutmak** (yaşam döngüsü bedava
   olurdu) — ELENDİ: generic satır mantığa ödünç verilemez. Bunun
   yerine `IdleClock` TEK kaynak oldu ve `binding`'in tam olarak aynı
   üç huninde (join / resume / despawn) açılıp kapanıyor.
5. **Saati her tick tüm üyeler için yansıtan bir per-tick projeksiyon**
   — ELENDİ: tick başına O(üye) kopya, 10k'da ölçülür.

**Saatte OLMAYANLAR.** Park edilmiş (detached) ve bot beslenen satırlar
saatten tamamen çıkarılır: ikisinin de canlı girdi kaynağı yoktur, READ
onlar için çekim yapmaz, ve bot girdisi mantığın `ingest`'i İÇİNDE
sentezlenir — aksiyon kanalını hiç geçmez, yani yukarıdaki yapısal
tanıma göre girdi değildir. Bu aynı zamanda AI devrinin bir AFK
bypass'ına dönüşmesini yapısal olarak engeller. Resume saati yeniden
başlatır. Shard geçişinde damga `PlayerMigration` ile TAŞINIR — yoksa
hareket eden boşta bir varlık her sınır geçişinde affedilirdi.

### 2. Tavan — `max_idle_input_secs` (varsayılan `None` = KAPALI)

Açıkken son aksiyon taşıyan karesi bu kadar eski olan üye, **ölü
taşımanın gittiği AYNI yola** verilir: yeni `detach_player` — tek işi
iki çağıran için TEK karar noktası olmak. Oyunun `on_disconnect`'i park
/ AI devri / despawn kararını verir; **base kendiliğinden hiçbir şeyi
despawn etmez.** Bu iki karar noktası yerine bir tane bırakır ve MOBA'ya
AFK'da bot devrini bedavaya verir.

Uyarı oda başına BİR KEZ (diğer zorlayıcı tavanların kuralı). Sayaç
olarak tutuluyor, bayrak olarak değil: `tracing` callsite ilgi
önbelleğini süreç genelinde tuttuğu için kapsamlı (scoped) bir abone ile
yakalama aynı ikili içindeki başka testlerden etkilenir — sözleşme
böylece deterministik biçimde kilitlenebiliyor.

`Some(0)` da KAPALI sayılır: sıfır tavan ilk süpürmede herkesi
düşürürdü, ki hiçbir operatör "0" ile bunu kastetmez.

**Tavanın elenen alternatifleri.**

1. **Base'in doğrudan despawn etmesi** — ELENDİ (sözleşmenin güçlü
   tavsiyesi): entity'nin kaderi için ikinci bir karar noktası açardı ve
   park/bot-devri hikâyesini baypas ederdi.
2. **Tavanı bağlantı aktörüne koymak** (soketi kapatıp ölü-taşıma
   yolunu tetiklemek) — ELENDİ: oda tarafında tick başına sıfır maliyet
   olurdu ama sinyal odada yaşıyor, tanım iki yere kopyalanırdı, ve
   sözleşme "shard aktöründe de aynala" diyor — yani iş aktörlerde.
   Bilinen sonucu §3'te (RECONNECT) belgelendi.
3. **Tavanın kendi `ExpireTo`/politika enum'unu taşıması** — ELENDİ:
   `on_disconnect` zaten tam olarak bu vokabüleri döndürüyor.

### 3. Tick başına maliyet

- **Tavan KAPALI (varsayılan):** tick başına BİR `Option` testi, üye
  başına sıfır. Artı gerçekten girdi TESLİM EDEN üye başına bir damga
  (hash arama + vektör yazımı) — sessiz üye hiçbir şeye mal olmaz, yani
  muhasebe boşta oyuncularla ölçeklenmez.
- **Tavan AÇIK:** saatin slot vektörü üzerinde sınırlı bir ROTASYON
  (adım başına en çok `SWEEP_BUDGET = 64` slot, kaldığı yerden devam —
  READ fazının döner imleç disiplini). Yani süpürme maliyeti üye
  sayısından BAĞIMSIZ sabit. Bedeli tespit gecikmesi: her üye bir
  rotasyon içinde (10k üye @ 30 Hz ≈ 157 adım ≈ 5 sn) incelenir — onlarca
  saniyelik bir tavanın yanında gürültü, ve bu bir TAVAN: geç davranmak
  güvenlidir, erken davranmak olmazdı.

### 4. Yan değişiklikler (gerekçeleriyle)

- **`RoomConn.identity`**: tavan, arkasında mesaj OLMAYAN bir detach
  sentezler; `on_disconnect`'e boş anahtar vermek idle-kick edilmiş
  oyuncuyu park edilemez ve resume edilemez kılardı (demo park defteri
  boş kimliği zaten reddediyor). Satır artık resume anahtarını
  hatırlıyor. Aynı sebeple `ShardMsg::Join` da onu taşıyor (kimlikli bir
  oyuncu, hiçbir defter tutmadığında sharded odaya BU koldan girer) ve
  `PlayerMigration` göçürüyor.
- **Zaten park edilmiş satıra gelen ikinci `Detach` yok sayılıyor**
  (politika iki kez sorulmuyor) — idle-kick edilmiş bir üyenin
  taşımasının sonradan ölmesi tam olarak bu şekildedir.
- **`ShardMsg::Migrate` artık `PlayerMigration`'ı kutuluyor**: iki yeni
  alan enum'u clippy'nin `result_large_err` eşiğinin üstüne itti, ki bu
  her link gönderiminin `Result`'ını o boyuta çıkarırdı.

### 5. Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **356 passed / 0 failed / 1 ignored**
- `gsb-loadgen -- 50 --duration 3` (tcp ve `--transport udp`) →
  `left=50`, `errors=0`, panik yok. Adım zamanları 6c0962d taban
  çizgisiyle aynı: `step_p50_us` dört koşuda da 130; ince histogramın
  p50'si taban 32 µs'ye karşı HEAD'de 40/24/32/32 µs (8 µs'lik tek bin,
  koşular-arası gürültü), UDP'de taban 32 → HEAD 32 µs.

### 6. Mutation-check

| Mutasyon | Kırılan |
|---|---|
| READ her ziyarette damgalıyor (`pulled > 0` guard'ı kalkıyor) | 4 test |
| Tavan tanımsızken varsayılan bir değere düşüyor | 3 test |
| Uyarı her seferinde basılıyor | 2 test (oda + shard) |
| Park kolu satırı saatten çıkarmıyor | 2 test (oda + shard) |
| `on_disconnect`'e boş kimlik veriliyor | 3 test |

Dördüncü mutasyon ilk denemede HAYATTA KALDI — süpürme satırı
`detach_player`'dan önce kendisi saatten çıkarıyordu, yani park kolunun
kendi `idle.stop`'u test edilemezdi. Gereksiz kopya kaldırıldı ve eksik
durum (GERÇEK taşıma ölümüyle park edilmiş, deadline'ı hiç olmayan bir
combat-held satırın tavan tarafından yeniden düşürülmemesi) kendi
testini aldı (`ffdedc0`).

### 7. Yapılmayanlar

- **Metrik sayacı eklenmedi.** Idle-expiry, oda örneklemine yeni bir alan
  eklemeden mevcut sayaçlarda görünüyor (politika `Despawn` derse
  `leaves`/`DetachDespawned`, `Hold` derse park sayaçları). Yeni bir
  `RoomSample` alanı, ROADMAP'te zaten açık olan `fold_rooms` denetimini
  büyütürdü; bilinçli kapsam dışı.
- **Soket kapatılmıyor.** Bkz. `docs/RECONNECT.md` §16.

## Kapatılanlar (bağlantı sınırları turu)

Teknik borç turunun ürün kararına bıraktığı **dört** maddeden **ikisi**
kapandı — ikisi de yeni bir makine değil, var olan bir mekanizmanın
simetrik tamamlanması:

1. **Tıkanmış yazmaya süre sınırı** (`write_stall_secs`) — reader
   pump'un idle saatinin soketin öteki yarısındaki eşi, ve rUDP REL
   bandının geçen turda inen canlılık sınırının TCP tarafındaki kardeşi.
2. **Post-auth HEARTBEAT_ACK kısması** — SECURITY §3.2 eşiğinin auth
   sınırının ötesine taşınması.

Açık kalan iki madde (AFK/zombi oturum politikası, geçerli girdinin
hacmi) bu turda **kasıtlı olarak ellenmedi**: ikisi de bir oynanış
parametresi seçmeyi ister, mevcut bir mekanizmayı tamamlamayı değil.

Test sayısı 340 → **344** (+4; hiçbir test silinmedi, gevşetilmedi,
`#[ignore]` eklenmedi — üç test SÖZLEŞME DEĞİŞİKLİĞİNE uyarlandı,
aşağıda). İki düzeltme de mutation-verified.

### 1. `5e17056` — tıkanmış giden yol ilerlemeye bağlandı

**Bulgu (koddan doğrulandı).** Giden yolun iki ucu zaten ele alınmıştı:
kanal DOLU ise oda batch'i düşürür ve sayar (tasarım gereği yavaş
istemci tolere edilir), kanal KAPALI ise bağlantı yıkılır (`w_closing`,
iki tur önce). Arada kalan durumun sınırı yoktu: okumayı bırakan bir
istemcinin alım penceresi kapanır → `FrameWriter::poll_ready`/`poll_flush`
`drain`'de `poll_write`'ı Pending görür → writer pump `sink.send().await`
içinde süresiz park eder → kanal DOLU kalır (asla KAPALI olmaz) → oda her
tick `dropped_frames` sayar → oturum hiçbir şey alamazken oda slot'unu ve
registry satırını tutar. Reader'ın idle penceresi bunu göremez: o GELEN
sessizliği izler, ve böyle bir istemcinin sessiz olması gerekmez.

**Turda çıkan, beklenenden kötü olan yan bulgu.** Kanal dolduğunda
zincir bununla da kalmıyordu: bağlantı aktörünün kendisi `send_frame`'in
`self.out.send(..).await`'inde park eder, inbox'ı dolar, ve reader pump
`in_tx.send(..).await`'te park eder — reader'ın deadline'ı yalnız
`stream.next()`'i sarar, `send`'i değil. Yani idle penceresi bu noktadan
sonra ARTIK KURULMUYORDU bile. Üç görev, iki kanal ve bir registry satırı
process ömrü boyunca kilitli kalıyordu; hiçbir katman fark edemezdi.

**Elenen alternatifler.**

- **Yaşa bağlamak** ("bir frame N saniyedir gönderilemedi"). Elenme
  gerekçesi: düşük-Hz odayı cezalandırır, yüksek-Hz'i ödüllendirir —
  aynı tıkanma 5 Hz'lik bir odada daha geç, 120 Hz'likte daha erken
  fark edilirdi; oysa ölçülen şey soketin durumudur, odanın temposu
  değil. Ayrıca istenen sözleşme (tıkla değil SÜREYLE) bunu zaten
  dışlıyordu.
- **Ardışık düşme sayısına bağlamak** ("oda üst üste N batch düşürdü").
  Elenme gerekçesi: aynı tick-bağımlılığı, artı yanlış katman — düşme
  ODANIN gözlemi, tıkanma ise SOKETİN durumu; kanal dolu ama soket
  boşalıyor olabilir (gerçek yavaş istemci) ve o istemci ölmemeli.
- **Bağlantı başına zamanlayıcı görev / süpürme mesajı.** Elenme
  gerekçesi: mimari kuralı (bağlantı başına yeni görev yok; aktörün tek
  await'i mailbox'ı) ve reader'ın zaten kanıtlanmış idiomu varken
  gereksiz — deadline tek awaited işlemin etrafına sarılır.
- **Aktörün `out.send`'ini `try_send`'e çevirmek.** Elenme gerekçesi:
  kontrol frame'lerini (AUTH_RESULT, ERROR) sessizce düşürürdü;
  tıkanmanın TEŞHİSİ değil, semptomunu gizlemek olurdu.

**Seçilen:** İLERLEME. Soketine `write_stall_secs` boyunca hiçbir şey
başarıyla yazılamamışsa yön ölüdür. Bu, ayırmak istediğimiz iki
istemciyi tam olarak ayırır: geride kalan ama HÂLÂ BOŞALTAN istemcinin
her yazması tamamlanır ve saati yeniden başlatır (sözleşme aynen durur,
düşen snapshot'ları eskisi gibi sayılır); hiç boşaltmayanınki hiç
tamamlanmaz. Varsayılan 10 sn: loopback/LAN'da soketin bir bayt bile
kabul etmediği on saniye yavaşlık değildir.

Mekanik reader'ın precedent'ini birebir izler — tek awaited işlem bir
deadline'a sarılır (`tokio::time::timeout` TEK future'a; ikinci canlı
kaynak değil), ve deadline son tamamlanan yazmadan bu yana pencereden
KALANI kadardır, yani N frame'lik bir batch N değil bir pencere alır.
Beklemek stall değildir: kanal boşken saat yeniden başlar.

Koşulun kendisinin dayattığı iki ayrıntı: stall çıkışı `sink.close()`
ÇAĞIRMAZ (nazik kapanış flush eder, ve çalışmayan şey tam olarak
flush'tır), ve raporlamadan ÖNCE giden kanalın alıcısını düşürür — böylece
aktörün kendi sonraki `send`'i, dolu olması bu pump'ın boşaltmayı
bırakmasından kaynaklanan bir kanalda park etmek yerine hızlıca hata
verir. Olağan çıkışlar da artık aynı pencere altında kapanıyor: çıkarken
tıkanan bir soket önceden writer görevini sonsuza dek tutabiliyordu.

Konfig `idle_timeout_secs`'in sözleşmesini birebir alır (f64 saniye,
`0` kapatır, kullanım anında map'lenir) ve yanına konur. `Endpoint::
start_pump` artık çıplak bir idle `Option<Duration>` yerine
`PumpTimeouts` çiftini alır: yön başına bir saat, ikisi de isimli.
rUDP ikisini de yok sayar (gelen sessizlik demux deadline heap'inin
işi, giden canlılık REL bandının ACK-ilerleme saati; datagram
`try_send_to` park etmez).

**Kilitler.** `gsb-net` `tcp::tests::stall`: GERÇEK loopback soketi,
bağlantıyı kabul edip hiç okumayan bir peer, ve writer'ın stall'ı
raporlayıp soket hiçbir şey kabul etmeden ÇIKIŞI. Tersi: boşlukları
pencereden UZUN olan bir damla akış asla stall sayılmaz (beklemek de
stall değildir). `write_stall.rs` uçtan uca: odaya giren, göndermeye
devam eden ve okumayı bırakan bir peer oturumunu ve registry satırını
kaybeder. Bu sonuncusu QUIC kapısından koşar — saat paylaşılan pump'ta
yaşadığı için her stream kapısı aynı kodu çalıştırır, ve alım
penceresini İSTEMCİ ayarlayabilen tek kapı QUIC'tir; böylece "okumayı
bıraktı" koşulu megabaytlarca çekirdek tamponu yerine kilobaytla
zorlanır.

**Mutation-check:** saati kapat → iki stall testi de kırmızı;
beklerken-sıfırlamayı kaldır → ters test kırmızı.

### 2. `0e1356e` — HEARTBEAT_ACK kısması auth sınırının ötesine

**Bulgu (koddan doğrulandı).** §3.2'nin 1/sn cevap eşiği yalnız
`ConnState::WaitingAuth`'a bağlıydı; auth başarısından sonra her
heartbeat koşulsuz cevaplanıyordu. Mimarinin tek 1:1 gelen→giden
dönüşümü, kimliği doğrulanmış istemcilere sınırsız açıktı.

**Endişe ölçüldü ve geçersiz çıktı.** ROADMAP maddesi kısmanın
"istemciye görünen semantiği değiştireceğini" söylüyordu. Düzgün bir
istemci saniyede bir heartbeat atar, yani eşiğin kendi temposundadır:
her cevabını ve `HeartbeatAck.tick`'ten okuduğu RTT'yi olduğu gibi alır.
Değişen tek şey, saniyede binlerce atan istemcinin artık aralık başına
bir cevap alması.

**Canlılık etkileşimi (doğrulandı).** idle penceresini sıfırlayan şey
frame'in GELMESİDİR, cevabı değil (`pump.rs` her okumayı yeniden sarar),
dolayısıyla cevapsız bırakılan bir heartbeat göndericisini asla sessiz
göstermez. Heartbeat'i başka hiçbir sunucu mekanizması okumaz.

**Elenen alternatifler.**

- **Fazlalığı ihlal bütçesine yazmak.** Elenme gerekçesi: pre-auth alan
  dokümanının gerekçesinin aynısı — kısma zaten maliyeti sınırlıyor
  (bağlantı başına aralıkta bir küçük ACK), o yüzden puanlamak düşman
  tarafında hiçbir şey kazandırmaz; yalnız gevşek bir NAT keepalive'ını
  ya da bozuk zamanlayıcılı bir istemciyi zorla düşürürdü. Tanımsız
  opcode bozuk/düşman istemci KANITIDIR; canlılık yoklaması değildir.
- **İki ayrı saat (faz başına bir tane).** Elenme gerekçesi: kısma TEK
  bir hız sınırlayıcıdır; iki saat iki mekanizma olurdu.
- **Sayaçları birleştirmek.** Elenme gerekçesi: ikisi farklı soruları
  farklı çarelerle yanıtlıyor. KİMLİKSİZ bir peer'ın fazlalığı §3.2'nin
  güvenlik sinyalidir (yanında §3.3 bütçesi ve §4 cap'i vardır; çare bir
  pre-auth guardrail'i sıkmaktır); kimliğini kanıtlamış bir peer'ınki
  BİLİNEN bir istemcinin heartbeat zamanlayıcısının bozuk olduğunu
  söyler — bir hata raporu. Birleştirmek, tek bir u64 kazanmak için
  §3.2'nin sayacına kendi sorusunu yanıtlatamaz hale getirirdi.
- **Auth'ta saati sıfırlamamak.** Elenme gerekçesi: authenticated
  oturumun ilk heartbeat'i cevapsız kalabilirdi ve AUTH'tan hemen sonra
  yoklayan bir istemci sessizliği ölü sunucu diye okuyabilirdi. Tek
  sıfırlamanın bedeli bağlantı ömrü boyunca tam olarak bir fazladan
  cevaptır (AUTH bir kez başarılı olur; ikincisi hard ihlaldir), yani
  bir kaldıraç değildir.

**Seçilen:** tek saat (`last_hb_ack`, `HEARTBEAT_ACK_MIN_INTERVAL`), auth
başarısında bir sıfırlama (§3.3 bütçesini emekliye ayıran aynı faz
sınırı; artık iki auth yolunun paylaştığı `authenticated()` geçişi), iki
sayaç (`m_preauth_hb_extra` + `m_hb_extra`, ikisi de aktör-local ve
debug-log — pre-auth sayacının zaten sahip olduğu şekil).

**Sözleşme değişikliğine uyarlanan testler** (hiçbiri gevşetilmedi;
kendi özellikleri korundu):

- `security.rs::preauth_heartbeat_flood_is_counted_not_answered` — eski
  post-auth bölümü 1:1 cevabı iddia ediyordu; artık auth sınırının tek
  sıfırlamasını ve ardından eşiğin devam ettiğini pinliyor.
- `half_dead.rs::healthy_out_channel_keeps_the_session_alive` — kendi
  özelliği ("canlılık trafiği bağlantıyı kesmenin gerekçesi olamaz")
  korundu: oturum tüm fırtınayı atlatıyor ve aralıktan sonra yeniden
  cevaplıyor.
- `e2e.rs::active_heartbeat_survives_the_idle_window` — GÜÇLENDİRİLDİ.
  Koşu 3,5 → 5 sn, ve ack sayısı artık İKİ taraftan doğrulanıyor: hâlâ
  boyunca cevaplanıyor (alt sınır, eski iddia) ve gönderim hızıyla artık
  1:1 değil (üst sınır, yeni iddia). Böylece bu test kısma ile idle
  penceresinin dikişinin de kilidi oldu.

Yeni kilit:
`security.rs::postauth_heartbeat_flood_is_answered_once_counted_and_never_scored`
— bir cevap, otuz dokuz sessiz sayım, ve ihlal
bütçesinin on katı bir fırtınanın ardından oturumun yalnız `Shutdown` ile
bitmesi (puanlanmama özelliği).

**Mutation-check:** faz kapısını geri koy → iki crate'te üç test kırmızı.

### Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **344 passed / 0 failed / 1 ignored**
- `gsb-loadgen -- 50 --duration 3` → `left=50 errors=0`, panik yok;
  `--transport udp` ile de aynı. Registry özet satırları 114c827
  tabanıyla BİREBİR: TCP `conns=0 opens=50 closes=50`, rUDP
  `conns=50 opens=50 closes=0` (rUDP'de FIN yoktur — oturumlar demux'un
  idle süpürmesine kadar yaşar; 3 sn'lik koşu onu görmez). Yani yanlış
  pozitif bir stall öldürmesi yok: 50 istemcinin hiçbiri eşiğin
  yakınından geçmiyor (istemciler sürekli okuyor; `out_bps_per_conn`
  ~11 KB/s).

## Kapatılanlar (minimum sayaçlar turu)

Bir önceki turun kardeş-alan denetiminin bıraktığı açık bulgu kapandı:
`step_min_us` ve `late_min_us` **hiçbir aktörde minimum değildi**. İkisi
de yalnız `if self.steps == 1` altında atanıyordu ve onları aşağı çeken
kol yoktu — yani her biri ilk adımın değerini process ömrü boyunca
taşıyordu. İlk adım tipik olarak en soğuk ve en yavaş olanıdır, yani
alanlar yalnız bayat değil, **dağılımın YANLIŞ UCUNU** raporluyordu; hem
de yanında gerçekten maksimum olan bir `step_max_us` dururken.

Test sayısı 330 → **340** (+10; hiçbir test silinmedi, gevşetilmedi,
`#[ignore]` eklenmedi — bir test DÜZELTİLDİ, aşağıda). Düzeltme
mutation-verified.

### Kullanıcı kararı: ONARIM (yeniden adlandırma ELENDİ)

ROADMAP maddesi kararı açık bırakmıştı: alanı düzeltmek mi, yoksa
dürüstçe `step_first_us` diye yeniden adlandırmak mı.

**Elenen alternatif — `step_first_us` / `late_first_us`.** Ucuz ve
yayınlanmış metriğin değerini değiştirmiyor. Elenme gerekçesi: yanındaki
`step_max_us` GERÇEK bir maksimum. Aynı satırda, aynı Prometheus
ailesinde, farklı anlama gelen bir "min" her okuyucuyu yanıltır — ve
zaten istenen ölçüm bir minimumdu, bir ilk-gözlem değil. Ayrıca
"ilk adımın süresi" operasyonel olarak neredeyse değersiz bir sayıdır
(bir kez ölçülür, bir daha asla değişmez), oysa minimum her turda
"bu oda ne kadar hızlı olabiliyor" sorusunu yanıtlar.

**Seçilen:** alanlar gerçek minimum yapıldı. Yayınlanmış metriğin anlamı
değişti (dünkü değer "ilk adım", bugünkü "minimum") — bu turun tek
bilinçli kırıcı değişikliği. Prometheus `# HELP` metinleri
DEĞİŞTİRİLMEDİ: ikisi de zaten "Minimum ..." diyordu, yani belge doğru,
yalan söyleyen koddu.

### Kommitler

1. **`98e35e4` — minimum sayaçlar gerçekten minimum.**

   **Bulgu (koddan doğrulandı):** `room/actor/lifecycle.rs:202` ve
   `shard/actor/lifecycle.rs:222` — dört alan (iki aktör × `step`/`late`)
   yalnız seeding kolunda atanıyor, `else if` kolu sadece maksimumu
   yükseltiyor. Loadgen çıktısında olgu çıplak, hatta beklenenden daha
   keskin: düzeltme öncesi TCP koşusu `step_min_us=264 step_mean_us=41.1
   step_max_us=264` — **min ile max BİREBİR aynı**, çünkü soğuk ilk adım
   aynı zamanda koşunun en yavaş adımıydı. Düzeltme sonrası aynı koşu
   `step_min_us=9 step_mean_us=48.1 step_max_us=163`.

   **Muhasebe tek yere alındı.** Blok iki kez yazılıydı (ikinci kopya
   "the room actor's accounting, mirrored" yorumuyla), o yüzden kusur da
   iki kez taşınıyordu. `RoomCounters::observe_late_us` /
   `observe_step_us` — `room/counters.rs`'in **ÇOCUK** modülü
   (`counters/observe.rs`), yani sayaç alanları private kalıyor. İki
   aktör artık aynı kodu çağırıyor: extremumlar, toplam ve iki histogram
   tek ölçümden. Tamsayı, allocation yok, await yok — tick gövdesi
   senkron kalıyor.

   **Seeding KORUNDU, ve bu fiksin asıl tuzağı odur.** İlk gözlem iki
   ucu da SET eder; sıfırdan başlayıp `min = min.min(x)` yazmak minimumu
   sonsuza dek 0'da bırakırdı — orijinal buglardan DAHA kötü, çünkü 0
   makul bir süre gibi okunur, oysa soğuk ilk adım gözle fark edilir.
   Ayrı bir testle kilitlendi.

   **İKİNCİ, GİZLİ ÖRNEK (toplama katmanı).**
   `loadgen::report::fold_rooms` `step_min_us`'i `min` ile katlıyordu ama
   `late_min_us`'e **hiç dokunmuyordu**. Akümülatör `*first`'ten
   başladığı için sharded bir oda, shard 0'ın gecikme minimumunu odanın
   minimumu diye raporluyordu (shard'lar örnek id'sine göre sıralı —
   `room << 16 | index` — yani hep shard 0). Bir minimumun katlaması
   `min`'dir. Düzeltildi.

   **TÜM MİN-BENZERİ ALAN ENVANTERİ** (metrics yüzeyi, room, shard, conn,
   registry, net, udp client stats tarandı):

   | Alan | Hüküm |
   |---|---|
   | `step_min_us` (oda + shard) | **KUSUR — düzeltildi** |
   | `late_min_us` (oda + shard) | **KUSUR — düzeltildi** |
   | `fold_rooms` → `late_min_us` | **KUSUR — katlanmıyordu, düzeltildi** |
   | `fold_rooms` → `step_min_us` | zaten doğru (`min`) — teste bağlandı |
   | `fold_rooms` → `hz` (`min`) | doğru, kasıtlı: en yavaş shard odanın hızıdır |
   | `MetricAccumulator::report` → `latest.*_min_us` | doğru: her shard KENDİ RoomId'siyle örnek gönderir, aktörler arası birleştirme burada olmaz; alan aktörde kümülatif |
   | `ClientReport::seq_first` | **"ilk" KASITLI** — bir extremum değil, hız penceresinin bir ucu (`(last−first)/Δt`) |
   | `*_max_us`, `snap_bytes_max`, `max_group`, `ack_lag_max_ms`, `ack_processed_max` | doğru: 0'dan başlayıp yükseliyorlar, bir maksimum için doğru başlangıç |
   | `UdpClientStats` (`retrans_out`, `dup_in`, `oob_dropped`, `gave_up`) | min yok, hepsi kümülatif sayaç |
   | RegistrySample, NetReport, ConnSample | min yok |

   **Bu turda DÜZELTİLMEYEN komşu bulgu (kayda geçiriliyor):**
   `fold_rooms` `late_mean_us`'i de katlamıyor — folded rapor ilk
   shard'ın ortalamasını taşıyor. Gerçek bir kusur, ama bir ORTALAMA,
   bir minimum değil; bu turun sözleşmesi minimumlardı. ROADMAP'e kendi
   maddesi olarak yazıldı; aynı denetimde `fold_rooms`'un `budget_us`,
   `req_*` ailesi, `metrics_dropped`, `pending_requests` ve `*_s`
   oranlarını da katlamadığı görüldü — toplu bir "fold denetimi" turu
   hak ediyor.

   **DÜZELTİLEN test (silinmedi, gevşetilmedi — GÜÇLENDİRİLDİ):**
   `sharded_step_fills_the_fine_duration_histogram`
   (`shard/tests/metrics.rs`). Bu test p50'sini bilerek `step_min_us` ile
   ALTTAN sınırlamıyordu ve yorumu bunun sebebini yazıyordu: "o alan bir
   minimum değil". Yani test, buglı semantiği belgeleyen bir yorumla
   birlikte yaşıyordu — ve doğru kodda o bracket geçerdi, buglı kodda
   geçmezdi. Artık bracket var, bin granülerliğinde (`fine_hist_
   percentile_us` bin'in ALT KENARINI döndürür — belgelenmiş ±8 µs), yani
   iddia aynı beş gözlem üzerinde kesinlikle daha güçlü.

   **Yeni testler (hepsi düzeltmeden ÖNCE kırmızı):**

   - `room/counters/tests.rs` (5 test): sentetik sürelerle tam-değer
     kilidi. Azalan-sonra-artan dizide minimum ORTADA (ne "ilkini tut"
     ne "sonuncusunu tut" geçebilir); ilk gözlem iki ucu seed eder;
     extremumlar iki histogram ile uyuşur; cap üstü adım skalerleri yine
     hareket ettirir.
   - `a_faster_tick_lowers_the_rooms_late_minimum` /
     `..._the_shards_late_minimum`: GERÇEK aktörler, doğrudan
     `step()`lenerek. Gecikme `adım başlangıcı − t.at` olduğu ve testi
     `t.at`'e sahip olduğu için "sonraki, daha hızlı gözlem raporlanan
     minimumu düşürür" iddiası DETERMİNİSTİK — scheduler ile yarış yok.
   - `duration_counters_stay_mutually_consistent`: 64 gerçek adımda
     min ≤ ortalama ≤ max, ve gözlem varken minimum 0 değil.
   - `folding_shards_takes_the_minimum_of_every_minimum` (loadgen bin
     içi unit test): küçük değer ÜÇ shard'ın İKİNCİSİNDE, yani ne
     "ilkini tut" ne "sonuncusunu tut" geçer.

   **Mutation-check:** step `min` kolunu sil → 4 test kırmızı; late `min`
   kolunu sil → 4 test kırmızı; seeding kolunu sil (minimum 0'da kalır)
   → **8 test kırmızı**; fold'daki `min`'i `max` yap → fold testi
   kırmızı.

### Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **340 passed / 0 failed / 1 ignored**
- `gsb-loadgen -- 50 --duration 3` → `left=50 errors=0`, panik yok

  | | `step_min_us` | `step_p50_fine_us` | `step_max_us` |
  |---|---|---|---|
  | TCP önce | 264 | 32 | 264 |
  | TCP sonra | **9** | 40 | 163 |
  | UDP önce | 210 | 40 | 210 |
  | UDP sonra | **13** | 24 | 233 |

  Önceki satırlarda `min == max` olması tesadüf değil: soğuk ilk adım
  koşunun en yavaş adımıydı, yani "minimum" tam olarak maksimumu
  gösteriyordu. Sonraki satırlarda minimum medyanın altında — sözleşme.

- aynısı `--transport udp` ile → `left=50 errors=0`, panik yok

## Kapatılanlar (park sızıntısı + shard metrik boşluğu turu)

İki bağımsız madde. Birincisi bir önceki turun (`f966164`, "park
expiry") **yarım kaldığının** tespiti: registry satırı yalnız hold
dolduğunda bırakılıyordu, politika park etmeyi REDDETTİĞİNDE değil — ve
reddetme, varsayılan konfigürasyonda (`disconnect_grace_secs = 0`) her
kopmada olan şeydir. İkincisi ROADMAP/HANDOFF'ta açık yan bulgu olarak
duran shard `step_fine_hist` boşluğu.

Test sayısı 327 → **330** (+3; hiçbir test silinmedi, gevşetilmedi,
`#[ignore]` eklenmedi). Her iki düzeltme de mutation-verified.

### Kommitler

1. **`df948c8` — politika park etmeyi reddettiğinde registry satırı
   bırakılır.**

   **Bulgu (adım adım kodda doğrulandı):**

   - `Registry::on_conn_closed` (`registry/actor/conns.rs`) bağlı bir
     satırı `detached` işaretleyip **tutar**. Bu işaret
     **SPEKÜLATİFTİR**: odanın politikası daha cevap vermeden yazılır,
     çünkü registry artık entity'nin kaderine karar vermez (RECONNECT
     §3/§4).
   - Odanın `Detach::Despawn` kolu (`room/actor/control.rs`) —
     `on_disconnect`'in park etmeyi reddettiği kol — `despawn_conn` ile
     olağan leave hunisinden geçiriyor ve registry'ye **hiçbir şey**
     söylemiyordu.
   - Bırakma raporunun tek göndericisi faz-0c hold süpürmesiydi
     (`room/actor/tick/detach.rs`, `shard/actor/tick/detach.rs`).
     Hiç başlamayan bir park'ın hold'u ve deadline'ı yoktur; süpürme o
     satır için **asla** çalışamaz. `grep -rn` ile başka gönderici
     olmadığı doğrulandı.
   - Satırın diğer iki bırakma yolu (aynı kimlikle resume, odanın
     bitmesi) geri dönmeyen bir oyuncuya ve kalıcı bir odaya hiç uğramaz.

   Sonuç: satır `room = Some(..)` + `detached = true` ile sonsuza kadar
   duruyordu. `Registry::room_members` (`registry/actor.rs`) onu saymaya
   devam ettiği için oturum, çoktan gitmiş bir entity adına hem bir
   `max_players` hem bir `max_connections` slotu tutuyordu. Yazının
   iddia ettiği üretim senaryosu (`disconnect_grace_secs = 0.0`, bağlan,
   AUTH+JOIN, `members == 1`, bağlantıyı düşür, `members` 1'de kalır)
   **testte birebir tekrarlandı** ve düzeltmeden önce kırıldı.

   **Elenen alternatif 1: registry kendi zamanlayıcısıyla eskitsin.**
   Reddedildi — hem karar hem grace oda tarafı politikadır (logic
   oyuncu başına seçer, combat-held bir park'ın deadline'ı hiç yoktur),
   yani registry tam da önemli olan vakalarda yanlış olurdu. Raporu
   olayı gören aktör verir.

   **Elenen alternatif 2: `ConnClosed` satırı hemen silsin (spekülatif
   işareti hiç yazma).** Reddedildi — park mekanizmasının tamamını
   yıkar: hold gerçekten başladığında satır ve slot RECONNECT §4 gereği
   tutulmalıdır.

   **Elenen alternatif 3 (isimlendirme): `ParkExpired`'ı olduğu gibi
   yeniden kullan.** Reddedildi. İsim yalnız süpürme için basılmıştı ve
   yeni göndericide yalan okunuyor: orada park EDİLMEDİ ve hiçbir şey
   "expire" etmedi — politika reddetti. "Park expired", park'ın hiç var
   olmadığı bir kolu anlatıyorsa bu, deponun karşı yazdığı türden
   sürüklenmedir.

   **Elenen alternatif 4 (isimlendirme): kardeş mesaj ekle
   (`ParkExpired` + `DetachDespawned`).** Reddedildi. Registry'nin
   gördüğü OLGU tektir — "bu detached satırın entity'si despawn edildi"
   — ve registry kolu iki gönderici için harfi harfine aynıdır. Tek
   eylem için iki isim, sürüklenmenin öbür yönüdür.

   **Seçilen:** `RegistryMsg::ParkExpired` → **`DetachDespawned`**
   (aktörlerdeki `park_reports` → `despawn_reports`). İsim artık iki
   nedenden birini değil, ikisinin de bildirdiği olguyu anlatıyor.
   Varyant `pub` ama `gsb-core` dışında hiçbir yerde anılmıyor; dış
   kullanıcı yok.

   **Uygulama.** Her iki aktör de olguyu öğrendiği koldan bildiriyor:
   odanın CONTROL fazı (`room/actor/control.rs`) ve shard'ın
   `ShardMsg::Detach` işleyicisi (`shard/actor/messages.rs`). İkisi de
   mevcut rapor kuyruğunu besliyor; kuyruk faz 0c sonunda bir kez
   boşaltılıyor ve CONTROL/mailbox drenajı aynı tick'te daha önce
   çalıştığı için reddedilen park normalde olduğu tick'te bildiriliyor.

   **Dolu-mailbox semantiği** bilinçli olarak süpürmeninkiyle aynı:
   senkron `try_send` (tick/control gövdeleri await'siz kalır), **FULL**
   mailbox id'yi bir sonraki tick'e yeniden kuyruklar — düşürülen bir
   rapor tam da bu sızıntıyı yeniden açar —, **CLOSED** olan düşürür
   (registry gitmiştir; sızılacak tablo kalmamıştır). Kuyruğa yalnız
   registry VARSA yazılır, böylece direct-drive test rig'leri birikmez.

   **AI-handover kolu hâlâ bilinçli olarak bildirilmiyor:** o hold,
   entity bir bot altında CANLI, slotunu gerçekten tutarak ve hâlâ
   geçerli bir resume hedefi olarak biter (RECONNECT §9).

   Testler: `declined_park_releases_the_registry_row` ve
   `sharded_declined_park_releases_the_registry_row_and_the_member_slot`
   (`tests/reconnect.rs`). İkisi de düzeltmeden ÖNCE yazıldı ve
   `members: 1` ile kırıldı. Mutation-check **ayrı ayrı** yapıldı:
   odanın raporunu bastırmak yalnız birincisini, shard'ınkini bastırmak
   yalnız ikincisini düşürüyor; iki hold-expiry testi her iki
   mutasyonda da yeşil kalıyor (yani yeni testler yeni kolları
   kilitliyor, eskisini değil).

2. **`35a965b` — shard'ın adım yolunda ince histogram doldurulur.**

   **Bulgu:** `ShardActor::sample` (`shard/actor/lifecycle/sample.rs:37`)
   `step_fine_hist`'i gönderiyordu, ama shard tarafında onu artıran
   hiçbir şey yoktu — ince sabit-bin histogramı yalnız oda aktörü
   yazıyordu (`room/actor/lifecycle.rs:226`). Sharded bir oda alanı
   ilan edip sonsuza kadar sıfır dizi gönderiyordu.

   **Tüketicinin gerçekte ne yaptığı (varsayılmadı, okundu):**

   - Prometheus yüzeyi `gsb_room_step_duration_us` p50/p99 satırlarını
     `if let Some(us) = fine_hist_percentile_us(..)` arkasında basıyor
     (`metrics/prometheus.rs:323`) ve o yardımcı boş histogramda `None`
     dönüyor (`metrics/mod.rs:212`). Yani sharded oda için kuantil
     satırları **sessizce hiç basılmıyordu** — yanlış değil, YOK.
     İddia doğrulandı.
   - Loadgen özeti daha kötü: `report/result.rs:252` geri düşüşü
     `unwrap_or(FINE_HIST_CAP_US)` ile yapıyor, yani `step_p50_fine_us`
     / `step_p90_fine_us` her sharded oda için **4096** basıyordu:
     uydurulmuş ve "patolojik yavaş" okunan bir sayı. Yazının bilmediği
     ikinci tüketici.

   **Elenen alternatif: alanı shard örneğinden kaldır.** Görevin izin
   verdiği ikinci yol, ama koşulu sağlanmıyor: ölçüm shard'da ZATEN
   var — `step_hist`, `step_min/max/sum_us` ve `late_*`'ı besleyen,
   `step_phases` çevresindeki aynı `t0.elapsed()`. Var olan ölçümü
   silmek, olmayan bir eksikliği belgelemek olurdu.

   **Seçilen:** aynı üç satır shard'da da binliyor. Yalnız tamsayı, tek
   `saturating_add`, hot path'te float yok; cap'in üstündeki adımlar
   oda tarafındaki gibi dışarıda kalır. Loadgen raporundaki eleman-bazlı
   `step_fine_hist` toplaması (`loadgen/report.rs:78`) zaten vardı ve
   sıfır topluyordu.

   Test: `sharded_step_fills_the_fine_duration_histogram`
   (`shard/tests/metrics.rs`) — gerçek bir `ShardActor::step` üzerinde.
   İki mutasyonla doğrulandı: artırmayı kaldırmak adım-başına-bir
   sayımında, başka bir değeri binlemek `p50 <= step_max_us`
   tutarlılığında düşürüyor.

   **KARDEŞ ALAN DENETİMİ (bu turda DEĞİŞTİRİLMEDİ, bulgu olarak
   kayda geçiriliyor):** aynı muhasebe bloğu tarandı.

   - `step_hist`, `step_max_us`, `step_sum_us`, `late_max_us`,
     `late_sum_us`: iki aktörde de doğru, boşluk yok.
   - `step_min_us` ve `late_min_us` **hiçbir aktörde minimum değil.**
     İkisi de yalnız `if self.steps == 1` altında atanıyor ve onları
     aşağı çeken bir kol YOK — yani her biri ilk adımın değerini
     process ömrü boyunca taşıyor, üstelik ilk adım tipik olarak en
     soğuk ve en yavaş olanı. Bu turun loadgen çıktısında olgu çıplak
     duruyor: `step_min_us=176 step_mean_us=36.3 step_max_us=176` —
     ortalama, "minimum"un beşte biri. `gsb_room_step_min_us`,
     `gsb_room_late_min_us` ve loadgen'in `step_min_us=` satırı
     dolayısıyla minimum adı takmış ilk-adım göstergeleridir.
     Bu, madde 2'nin kusuru (shard'da hiç yazılmamak) DEĞİL; iki
     aktörün paylaştığı ayrı bir kusur. Kendi turuna ve kendi kararına
     bırakıldı — yeni testin yüzdelik iddiası da bilerek `step_min_us`
     ile alttan sınırlanmıyor (gerekçe test yorumunda).

### Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **330 passed / 0 failed / 1 ignored**
- `gsb-loadgen -- 50 --duration 3` → `left=50 errors=0`, panik yok
- aynısı `--transport udp` ile → `left=50 errors=0`, panik yok

rUDP koşusunun `server registry (final): conns=50 ... closes=0`
satırı bu turdan ÖNCE de aynıdır (taban `2791a79` ile koşularak
karşılaştırıldı): rUDP'nin EOF'u yoktur, oturumlar son rapordan sonra
idle-timeout ile ölür, yani `on_conn_closed` o pencerede hiç
çalışmamıştır — detach yolu ile ilgisi yoktur.

## Kapatılanlar (rUDP doğruluk turu — sessiz REL give-up, tekrar oynatılabilir cookie)

Kaynak: doğrulanmış dış inceleme raporundaki iki rUDP bulgusu. İkisi de
daha önce **bilinçli olarak ertelenmişti** (taşıma "experimental"
statüsüne alınmıştı); kullanıcı bu turda ikisinin de kapatılmasına karar
verdi. Taşıma **deneysel kalmaya devam ediyor** — mezuniyet ayrı bir
karar, kalan iş aşağıda madde madde doğrulandı.

Test sayısı 319 → **327** (+8; hiçbir test silinmedi, gevşetilmedi,
`#[ignore]` eklenmedi). Her iki düzeltme de mutation-verified: üçer
mutasyon denendi, her biri en az bir testi düşürdü.

### Kommitler

1. **`0a5a9cb` — REL bandı gerçekten güvenilir: sessiz give-up yerine
   ACK-ilerlemesi ölçen ölüm eşiği (madde A).**

   **Bulgu (kodda doğrulandı):** `writer.rs:174`, `retransmit_pass` —
   `RETRANSIT_MAX`'ten (250 ms) eski bir kontrol frame'i kuyruktan
   atılıyor, `gave_up` artırılıyor ve döngü devam ediyordu. Başka
   HİÇBİR ŞEY olmuyordu. Alıcının cumulative akışı deliğin ötesine asla
   geçemediği için o yön kalıcı olarak tıkanıyor, ama oturum yaşıyor ve
   RAW oyun bandı akmaya devam ediyor — yani tıkanma GÖRÜNMEZ. Kaybolan
   frame `JOIN_ROOM_RESULT` ise istemci sonsuza kadar bekler (hata yok,
   retry yok, kapanma yok) ve sunucu o oturumun tuttuğu her şeyi tutmaya
   devam eder.

   **Aday 1 (elendi): give-up ⇒ oturum-ölümcül.** Sonuç konusunda doğru
   (teslim edilemeyen bir kontrol frame'i oturumu bitirmeli), TETİKLEYİCİ
   konusunda yanlış: 250 ms'de, yani beş denemeden sonra ölüm ilan eder.
   Oysa 250 ms'lik kayıp kötü bir mobil hatta OLAĞAN bir olaydır — LTE
   handover onlarca ms, Wi-Fi roam 100-500 ms, Wi-Fi↔hücresel geçiş
   1-3 sn. Bu eşik bugün yalnızca *kekeleyen* oturumları öldürürdü:
   sessiz bir hata yerine gürültülü bir hata. `RETRANSIT_MAX`'i
   yaşanabilir bir değere çıkarmak ise onu bu tasarımın kötü isimli bir
   kopyası yapar — tek frame'in yaşı kanalın canlılığını ancak
   YAKLAŞIK ölçer ve patlamalı kayıpta kötü ölçer (4 sn sessizlikten
   sonraki ilk frame bütün karartmayı kendi yaşında taşır).

   **Aday 2 (seçildi): bellek-sınırlı yeniden gönderim + ACK-ilerlemesi
   olmayan süre eşiği.** Bir frame, bant yaşadığı sürece yeniden
   gönderilir; hiçbir zaman tek başına terk edilmez. BANT bütün olarak
   ölü ilan edilir: kuyrukta bir şey varken cumulative ACK
   `REL_NO_ACK_FATAL` (5 sn) boyunca HİÇ ilerlemediyse, ya da un-ACK'li
   kuyruk `RETRANSIT_CAP`'e (256 frame) ulaştıysa.

   **Eşik gerekçesi (5 sn).** Yukarıdaki bütün kekemelikleri payla
   geçer (Wi-Fi roam'un 5-15 katı, en kötü Wi-Fi↔hücresel geçişin ~2
   katı) ve 50 ms RTO'da aynı frame'in ~100 yeniden gönderimi demektir:
   5 sn içinde 100 denemede hiçbir şey teslim edemeyen bir yol
   kekelemiyor, ölü. Ayrıca demux'un 30 sn'lik `idle_timeout`'unun çok
   altında — ve o sweep GELEN sessizliği izler, yani RAW girdi göndermeye
   devam edip hiç ACK'lemeyen peer (raporlanan şeklin ta kendisi) ona
   görünmez.

   **Ölüm gürültülü ve oturum-ölümcül.** Writer, connection actor'ün
   MAILBOX'ına `ConnIn::ServerClosed` bırakır — süreç-içi bir kanal,
   ASLA soket (şüphe altında olan tam da o) — ve actor olağan teardown'ını
   koşar (son metrik flush'ı, `RegistryMsg::ConnClosed`). Oradan sonrası
   düşmüş bir TCP soketinden ayırt edilemez: registry, ilişkisiz bir
   oturumun satırını doğrudan bırakır; oda üyesi için DETACH yönlendirir
   ve entity ile slotun sahibi odanın `on_disconnect` politikası olur
   (RECONNECT §4).

   **Diğer elenenler:** (a) *RTO backoff + retry sayacı (TCP şekli)* —
   aynı sınırı kimsenin akıl yürütemeyeceği birimlerle ifade eder (kaç
   retry 5 sn eder? backoff eğrisine bağlı) ve backoff'un bir işe
   yaraması için RTT kestirimi gerekir, ki bu taşımada henüz yok;
   (b) *kapatmak yerine istemciye haber vermek* — haber verecek yol yok,
   ERROR'un kendisi kontrol bandı frame'idir ve deliğin arkasına
   kuyruklanır; RAW bir bildirim ise message-table dışı yeni bir opcode
   ve onu anlayan bir istemci ister (ölü bir oturumu biraz daha kibar
   yapmak için protokol değişikliği).

   **Bellek sınırı.** Per-frame give-up kalkınca kuyruğu 250 ms'lik saat
   sınırlamıyor; açıkça sınırlandı: yön ve oturum başına 256 frame.
   Kontrol frame'leri küçüktür (onlarca bayt), yani gerçekçi tavan birkaç
   KB; mutlak tavan (her frame tam datagram bütçesinde) ~380 KB ve ona
   yalnızca ACK'leri ZATEN durmuş, yani ölüm penceresine girmiş bir
   oturum ulaşabilir.

   **İstemci yarısı aynalandı** (tek frame terk etme yok, aynı eşik, aynı
   bellek sınırı); `is_established` `false`'a döner — UDP'de EOF olmadığı
   için bir çağıranın alabildiği TEK "oturum bitti" sinyali budur.
   `UdpClientStats::gave_up` adını korudu ve artık bant öldüğünde kuyrukta
   kalanı sayıyor (loadgen `RESULT` satırı değişmedi).

   **Testler:** bellek kolu `gsb-net`'te (el sıkışıp sonra hiç ACK'lemeyen
   peer; milisaniyeler) ve duvar-saati kolu uçtan uca `gsb-server`'da
   (`tests/udp_rel_liveness.rs`: ACK'lemeyi bırakan iki peer; registry
   ikisinin de olağan close'unu görüyor ve park edilmeyen satırı
   bırakıyor). Mutasyonlar: duvar-saati kolunu, bellek kolunu ya da
   actor'e haber vermeyi kapatmak — üçü de birer testi düşürdü.

   Writer'ın güvenilir-bant yarısı **ÇOCUK** modüle taşındı
   (`writer/reliable.rs`), iki dosya da 200-250 hedefinin içinde.

2. **`8c74d8b` — El sıkışma cookie'si artık son kullanma tarihli
   (madde B).**

   **Bulgu (kodda doğrulandı):** `cookie.rs`, `compute(&self, nonce,
   peer)` yalnız key, istemci nonce'u ve peer adresini karıştırıyordu.
   Zaman terimi YOK, rotasyon YOK: telden yakalanan bir proof proses
   ömrü boyunca geçerli kalıyor, yani anti-spoofing el sıkışması aynı
   görünen adresten süresiz tekrar oynatılabiliyordu.

   **Düzeltme:** `F`'ye dördüncü terim — bind'dan bu yana geçen
   `COOKIE_SLOT` (10 sn) periyodunun tamsayı sayacı olan bir **zaman
   dilimi**, doğrulama anında monotonik bir `Instant`'tan hesaplanır.
   Sunucu challenge'ı güncel dilim için üretir, proof'u güncel **ya da
   bir önceki** dilim için kabul eder.

   **Bilinçli olarak DEĞİŞMEYENLER:** (a) *tel* — hâlâ
   `3 HELLO [u64 LE nonce][u64 LE cookie]`, her yönde 18 bayt; slot
   gönderilmez (sunucunun kendi iki hesabı aynı saatten okur) ve
   cookie'den tek bit çalınmaz; (b) *statelessness* — el-sıkışma öncesi
   tablo yok, timer görevi yok, paylaşılan rotasyon durumu yok, kilit
   yok; demux tek-beklenen-kaynak özdeşliğini korur, rotasyonun maliyeti
   el sıkışma başına iki tamsayı bölmesi; (c) *key* — hâlâ entropi
   türevli, hâlâ bind'da BİR KEZ çekiliyor, hâlâ "entropi ya da başlama".
   Slot PUBLIC bir sayaçtır ve tam da public olduğu için key'le birlikte
   katlanır. Ayrım artık isimlerde de yaşıyor: `cookie_key_*` testleri
   SIRRI, `cookie_slot_*` / `cookie_proof_*` / `cookie_clock_*` testleri
   SON KULLANMA'yı kilitler; `cookie_key_is_not_derived_from_the_wall_
   clock` tek karakter değişmeden duruyor.

   **Aralık ve açık kalan pencere.** Proof, üretildiği dilimden sonraki
   dilimin sonuna kadar yaşar: pencere **10-20 sn**. İki yönden
   boyutlandı — *altında*, kırmaması gereken el sıkışma: proof,
   challenge alındıktan bir RTT + istemci zamanlayıcı gecikmesi sonra
   üretilir (kötü hatta ~300 ms, telsizi uykudan kalkan telefonda birkaç
   saniye), 10 sn bunun 3-30 katıdır ve meşru bir el sıkışma rotasyona
   yenilmez; yenilse bile istemcinin retry'si taze challenge alır (sunucu
   bunda idempotent). *Üstünde*, bıraktığı maruziyet: 10-20 sn, herhangi
   bir gerçekçi yakala→tekrar-oynat hattının çevrim süresinden kısadır.
   **Sınırı geçen el sıkışma:** challenge dilim N'de üretilip proof dilim
   N+1'de geldiğinde doğrulama önce N+1'i, sonra N'i dener ve KABUL eder
   — önceki-dilim toleransının tek varlık sebebi budur. N-2 ve öncesi
   reddedilir (`bad_cookie` sayılır, cevap verilmez).

   **Elenen alternatifler:** (a) *zaman damgasını cookie bitlerine
   gömmek* — 64 bitin 16'sını kaba üretim zamanına ayırıp tek dilim
   doğrulamak; aynı politikayı ifade eder ama cookie'nin dayandığı tek
   şeyi, sahte üretilemez değerin genişliğini, 64'ten 48 bite indirir;
   (b) *üretilen cookie'leri bir kümede tutmak* — tam tek-kullanımlık
   semantik, ama stateless el sıkışmanın var olma sebebi olan
   "doğrulanmamış peer başına tahsis yok" kuralını çiğner (saldırganın
   sahte challenge istekleri tabloyu boyutlandırır); (c) *key'i
   döndürmek* — gözlemlenebilir davranış aynı, ama sabitin yerine mutable
   bir sır koyar (aynı anda iki key yaşar, ikisi de demux görevinden
   yazılır, `bind`'ın entropi garantisi her çalışma-zamanı çekilişi için
   de geçerli olmak zorunda kalır). Slot terimi aynı son-kullanma'yı
   key'i DEĞİŞMEZ bırakarak sağlar.

   **Testler:** slot `F`'nin terimidir; süresi geçmiş dilimin proof'u
   reddedilir; bir rotasyonu geçen proof kabul edilir (ve tolerans dilim
   0'da sarmalanmaz); başka bir peer'ın proof'u her dilimde reddedilir;
   saat periyot başına bir dilim ilerler. Artı aynı süre-geçti/tolerans
   çifti demux'un gerçek `handle_hello`'sundan geçirilerek. Mutasyonlar:
   slotu `F`'den düşürmek, önceki-dilim toleransını kaldırmak, ya da
   geçmiş bütün dilimleri kabul etmek — üçü de birer testi düşürdü.

3. **Bu doküman commit'i — statü beyanı ve doküman çürüğü
   (maddeler C + D).**

   `udp/mod.rs`'in "Status: experimental" paragrafı yeniden yazıldı:
   kapanan iki boşluk adıyla anılıyor, ama **etiket kalıyor** —
   mezuniyet ürün kararı. Kalan iş yeni bir "What is still open"
   başlığında, her madde koda karşı DOĞRULANARAK listelendi:

   - **congestion control / pacing yok** — `udp/` altında token bucket,
     pacer ya da pencere yok; sunucu pps'sini sınırlayan tek şey odanın
     tick + snapshot bütçesi (doğrulandı: sıfır eşleşme);
   - **sabit RTO, RTT kestirimi yok** — `RETRANSIT_RTO` derleme zamanı
     50 ms ve hiç geri çekilmiyor; hiçbir yerde RTT örneği alınmıyor
     (doğrulandı: sabit, writer ve client tarafından olduğu gibi
     kullanılıyor). Liveness eşiğinde backoff şeklinin elenmesinin sebebi
     de bu;
   - **NAT rebinding oturumu bitiriyor** — `Demux::sessions` 4-tuple ile
     anahtarlı, `handle_hello` bilinen adres için erken dönüyor; rebind =
     yeni adres = yeni el sıkışma + yeni `ConnectionId` (doğrulandı);
   - **`SO_RCVBUF` ayarı yok** — tek soket her oturumu taşıyor, çekirdek
     alım kuyruğu patlamada ilk ve tek tampon ve sistem varsayılanında;
     `tokio::net::UdpSocket` (1.53.1) setter sunmuyor, raw fd gerekir
     (doğrulandı: koddaki tek iz `bind`'daki not);
   - **kripto katmanı ve parçalama yok** — ikisi de v1 kapsam DIŞI
     (bekleyen değil): cookie tek başına bir güvenlik sınırı değil, bütçe
     üstü datagram bölünmüyor, atılıp sayılıyor.

   **Doküman çürüğü (D).** `docs/ROADMAP.md:78` "Test sayısı: bugün
   itibarıyla **314**" diyordu; main'deki gerçek sayı **319**'du (teknik
   borç turu beş test ekledi, bu satırı güncellemedi — README doğruydu).
   Satır bu turun sonundaki **327**'ye çekildi ve tarihsel zincire
   314 → 319 eklendi. Aynı taramada ikinci bir çürük yakalandı:
   `loadgen/churn.rs` gömülü-boşluk maddesi P3'te hâlâ `[ ]` işaretliydi,
   oysa teknik borç turu (`4465c47`) onu kapatmıştı — kodda doğrulandı
   (dosyada 38 boşlukluk koşu kalmadı), madde `[x]`'e çekildi. ROADMAP'in
   rUDP give-up ertelemesi de artık erteleme değil: bu tura işaret
   ediyor. README/HANDOFF sayıları 327'ye, README'nin taşıma satırı iki
   yeni davranışa (cookie rotasyonu, REL bandının oturum-ölümcül ölümü)
   güncellendi.

### Doğrulama

- `cargo fmt --all --check` → temiz
- `cargo clippy --workspace --all-targets -- -D warnings` → 0 uyarı
- `cargo test --workspace` → **327 passed / 0 failed / 1 ignored**
  (gsb-lint doctest)
- `cargo run --release -p gsb-server --bin gsb-loadgen -- 50 --duration 3`
  ve aynısının `--transport udp` varyantı: ikisinde de `left=50`,
  `errors=0`, panik yok (ham satırlar aşağıda).

TCP (`50 --duration 3`):

```text
RESULT mode=in-proc visibility=all shards=1 max_snap_bytes=1400 clients=50 connected=50 joined=50 left=50 snap_total=4100 snap_per_client_p50=82.0 tick_hz_med=30.00 client_in_bps=556983 client_out_bps=3867 out_bps_per_conn=10977 moves=850 errors=0 steps=90 server_hz=30.02 step_p50_us=130 step_p50_fine_us=24 step_p90_fine_us=48 step_max_us=192 step_over_budget_pct=0.0 dropped=0 late_max_us=127 peak_payload_b=402 snap_overflows=0 records_per_tick=0.0 overlap_x=0.00 server_in_bps=2534 server_out_bps=548833 peak_conns=50 metrics_dropped=0 profile=ring offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=800 ack_processed_max=16 ack_lag_max_ms=34 fulls=4100 private_fulls=0 deltas=0 gap_drops=0 view_size=2500 still_frac=0.9 req_local=0 req_ext=0 req_rej_malformed=0 req_rej_dup=0 req_rej_no_handler=0 req_rej_logic=0 req_rej_conn=0 req_rej_room=0 req_to=0 req_late=0 req_pending=0 churn_cycles=0 resumed=0 fresh_joins=0 room_resumes=0 resume_rejected_stale=0 detach_expired_ai=0 detach_expired_despawn=0
```

rUDP (`50 --duration 3 --transport udp`) — (A)'nın en olası regresyon
yüzeyi; istemci tarafı sayaçlar temiz (`retrans_out=0 gave_up=0`), yani
kayıpsız loopback'te yeni ölüm eşiği hiç tetiklenmiyor:

```text
RESULT mode=in-proc visibility=all shards=1 max_snap_bytes=1400 clients=50 connected=50 joined=50 left=50 snap_total=4100 snap_per_client_p50=82.0 tick_hz_med=30.00 client_in_bps=556683 client_out_bps=3068 out_bps_per_conn=11028 moves=850 errors=0 steps=90 server_hz=30.01 step_p50_us=130 step_p50_fine_us=32 step_p90_fine_us=72 step_max_us=203 step_over_budget_pct=0.0 dropped=0 late_max_us=15 peak_payload_b=402 snap_overflows=0 records_per_tick=0.0 overlap_x=0.00 server_in_bps=2501 server_out_bps=551400 peak_conns=50 metrics_dropped=0 profile=ring offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=udp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=800 ack_processed_max=16 ack_lag_max_ms=34 fulls=4100 private_fulls=0 deltas=0 gap_drops=0 view_size=2500 still_frac=0.9 req_local=0 req_ext=0 req_rej_malformed=0 req_rej_dup=0 req_rej_no_handler=0 req_rej_logic=0 req_rej_conn=0 req_rej_room=0 req_to=0 req_late=0 req_pending=0 churn_cycles=0 resumed=0 fresh_joins=0 room_resumes=0 resume_rejected_stale=0 detach_expired_ai=0 detach_expired_despawn=0
```

## Kapatılanlar (teknik borç turu — ölü metrik, yarı-ölü bağlantı, post-auth girdi)

Kaynak: doğrulanmış üç borç (a-c) + bir kozmetik (d). Her madde **önce
kodda doğrulandı**; iki yerde doğrulama iddiayı DEĞİŞTİRDİ (b'de ROADMAP
metni yanlıştı, c'de maruziyet iddia edilenden farklı bir yerdeydi).
Test sayısı 314 → **319** (+5; hiçbir test silinmedi, `#[ignore]`
eklenmedi — bir testin harness'i düzeltildi ve bir vakum assertion
gerçek bir assertion'la değiştirildi, ikisi de aşağıda gerekçeli).

### Kommitler

1. **`4465c47` — Sarmalanan churn log literal'i onarıldı (madde d).**
   `loadgen/churn.rs`'teki stale-resume mesajı üç kaynak satırına `\`
   devamı olmadan sarılmıştı, yani her satırın girintisi string'in
   İÇİNE girmişti: basılan metin ~38 boşlukluk iki koşu taşıyordu.
   rustfmt string literal'lerini hiç yeniden yazmadığı için her format
   turundan sağ çıkmıştı. `\` devamıyla düzeltildi (satır sonunu ve
   sonraki satırın baştaki boşluğunu yutar) — ağaçta zaten kullanılan
   biçim. Başka hiçbir kod yeniden akıtılmadı.

2. **`dd064e9` — Yapısal olarak hep-sıfır oda sayacı emekli edildi
   (madde a).** `RoomCounters::dropped_actions` 0'a kuruluyor ve ne oda
   ne shard aktörü tarafından **hiç** yazılmıyordu; buna rağmen
   `scope=room` log satırında basılıyor ve Prometheus yüzeyinden
   `gsb_room_dropped_actions_total` olarak, "Input actions dropped on
   READ-channel overflow" HELP metniyle **dışa veriliyordu**. Operatör
   kalıcı sıfırı "girdi hiç düşmüyor" diye okur — sunucunun iddia
   edecek durumda olmadığı bir cümle.

   **Seçenek 1 (sayacı gerçek düşme noktasında doğru bağla) zaten
   yapılmıştı** — başka yerde ve daha iyisiyle: tek girdi-kaybı noktası
   bağlantı aktörünün odanın aksiyon kanalına yaptığı `try_send`'idir ve
   orası `m_actions_dropped` → `ConnSample::actions_dropped` →
   `NetReport::actions_dropped` → `gsb_net_actions_dropped_total`
   zincirini, üstelik `actions_dropped_top` ile **kime ait olduğu**
   bilgisiyle birlikte zaten besliyor. Aynı olayı ODA kapsamına taşımak
   ya odanın control kanalını (zaten toplanmış bir sinyal için hot
   path'e ek iş) ya da tick gövdesine await (yasak) gerektirirdi — ve
   **yanlış atfederdi**: aksiyonu oda düşürmedi, gönderen düşürdü.
   Odanın READ fazı sınırlı bir *çekme*dir ve **erteler**; oda kapsamlı
   bir düşme sayacı bağlanmamış değil, ilkesel olarak bağlanamaz.

   Bu yüzden seçenek 2: sample'dan, rapordan, log satırından ve
   Prometheus yüzeyinden kaldırıldı. Giden-batch düşme ailesi
   (`gsb_room_dropped_total` / `_s`) gerçekten yazılan ayrı bir sayaçtır,
   dokunulmadı. Loadgen metrik çerçevesi alanı birlikte düşürdü; magic
   GSM5 → **GSM6** (iki uç da aynı binary'de, format kayamaz).

   **Envanter (istenen tarama):** `RoomCounters`'ın 39 alanı tek tek
   mutasyon noktası için tarandı — `dropped_actions` **tek** yazılmayan
   alandı; kalan 38'inin hepsinin oda ve/veya shard tarafında en az bir
   artırma noktası var. Shard tarafı aynı `RoomCounters` tipini
   paylaşıyor, yani aynı yalanı o da veriyordu ve aynı kaldırmayla
   kapandı. **Yan bulgu (düzeltilmedi, rapor edildi):**
   `step_fine_hist` shard aktöründe hiç yazılmıyor (oda
   `lifecycle.rs`'te yazıyor, shard `lifecycle.rs`'te yok) — sonuç
   "sıfır sayaç" değil, `gsb_room_step_duration_us` p50/p99 satırlarının
   sharded odalar için **hiç basılmaması** (renderer yok değeri tahmin
   etmek yerine satırı atlıyor). Ayrı bir borç.

   Kilit: `input_drops_are_exported_only_at_the_net_scope` — net kapsam
   gerçek toplamı atfıyla taşıyor, iki yüzeyin hiçbiri oda kapsamlı bir
   girdi-düşme anahtarı ilan etmiyor. Mutation check: Prometheus
   ailesini (`|_r| 0` ile) geri koymak testi kırıyor; log satırına
   `dropped_actions=` anahtarını geri koymak da.

   **`read_fairness.rs` notu:** oradaki `dropped_actions == 0`
   assertion'ı **iki kez birden vakumdu** — alan hiç yazılmıyordu VE
   okuduğu kanal step barrier'ı tarafından zaten boşaltılmıştı, yani
   boş kuyrukta geçiyordu. Yerine gerçek bir assertion kondu: son
   barrier örneği odanın her paced tick'te bir adım tamamladığını
   gösteriyor (flood odayı durdurmadı). Testin asıl davranış kilidi (her
   bağlantı bir rotasyon içinde erişiliyor + "overload gerçekti"
   koruması) olduğu gibi duruyor.

3. **`3c4e32b` — Giden yolu ölmüş bağlantı düşürülüyor (madde b).**
   ROADMAP'in tarif ettiği boşluk **yanlıştı** (madde metni bu turda
   düzeltildi): "istemci heartbeat atıyor ama karşı taraf gitmiş"
   tutarsız bir durum, ve önerilen çözüm (heartbeat son-görülme damgası
   + Phase 0c süpürmesi) bir no-op'tu — heartbeat mevcut idle
   penceresini sıfırlayan şeyin ta kendisi.

   Gerçek boşluk soketin **yazma** yarısındaydı ve üç katman birden
   görmezden geliyordu: writer pump yazma/flush hatasında çıkar ama
   elinde inbox yok (sessiz); oda fan-out'u `TrySendError::Closed`'ı
   `Full` ile aynı sayıp sonsuza kadar tekrar dener; bağlantı aktörü de
   `let _ = self.out.send(..)` ile sonucu atıyordu. Bir daha tek bayt
   alamayacak oturum registry satırını ve `max_players` slot'unu
   tutuyordu — varsayılanda reader'ın 30 sn penceresi süpürene kadar,
   `idle_timeout_secs = 0` ile sonsuza kadar.

   Bounded bir `send` yalnızca kanal KAPALIYKEN `Err` verir ve tek
   alıcısı o bağlantının writer pump'ıdır; yani `Err` bir politika
   yargısı değil, kesin bir olgudur. `w_closing` ile kaydediliyor, run
   loop mevcut `v_closing`/`p_closing` yanında kırılıyor — olağan
   teardown kaskadı, kapanış bildirimi yok (gönderecek yol kalmadı),
   yeni mesaj sınıfı yok, süpürme yok, zamanlayıcı yok, hiçbir tick
   gövdesine await eklenmedi.

   **Elenen alternatifler:** (i) ROADMAP'in heartbeat damgası + Phase 0c
   besleyicisi — no-op olduğu için elendi (aynı olay, ikinci saat); (ii)
   `Full` durumunu da düşürmek — "yavaş istemci tolere edilir"
   kararıyla çelişir, bir eşik sayısı ister, ürün kararı olarak açık
   bırakıldı; (iii) `SO_KEEPALIVE` (socket2) — ortogonal ve ucuz ama
   yeni bağımlılık + ayrı bir savunma hattı, bu turun kapsamı değil.

   Kilit: `tests/half_dead.rs::closed_out_channel_tears_the_session_down`
   (auth sonrası, out alıcısı düşürülmüşken sıradaki heartbeat
   `RegistryMsg::ConnClosed` üretiyor ve aktör çıkıyor). Mutation check:
   `let _ =`'i geri koymak testi o rapor için zaman aşımına düşürüyor.
   Tersi `healthy_out_channel_keeps_the_session_alive`: erişilebilir
   bağlantı sekiz heartbeat'i cevaplıyor ve hiç kapalı raporlanmıyor —
   düzeltme "her yazmada kapat"a dejenere olamaz.

4. **`f0d3ad7` — Tanımsız game-band opcode'lar ihlal bütçesine yazılıyor
   (madde c).** Maruziyet ölçüldü: üç pre-auth limitinin üçü de
   `ConnState::WaitingAuth`'a bağlı, auth sonrası tamamen kapanıyor.
   Kalan gerçek açık **bant sınırının çöpü bedava yapmasıydı**:
   bilinmeyen *base-band* opcode cevaplanıp bütçeleniyor (dördüncüde
   kapanış), ama tanımsız bir *game-band* opcode körlemesine
   iletiliyor, odanın tick bütçesinden **çekiliyor** ve ancak oyunun
   ingest'inde sessizce atılıyordu — cevap yok, puan yok, sınır yok.

   Mesaj tablosu sunucunun tel sözleşmesidir (DESIGN §5; `wire_contract
   .rs` emekli numaraların orada asla görünmemesini zaten garanti eder),
   dolayısıyla tabloda olmayan bir opcode bu sunucunun konuşmadığı bir
   mesajdır — base-band kuralının dayandığı önermenin aynısı. Mevcut
   `reply_err(UnknownOpcode)` → ağırlıklı ömür bütçesi yeniden
   kullanıldı; `is_registered` zaten vardı. Kontrol oda-durumu testinden
   ÖNCE koşuyor, böylece sınıflar ayrı kalıyor: tanımsız opcode oda
   durumundan bağımsız düşmancadır, KAYITLI bir game op'un leave sonrası
   gelmesi ise olağan ~1-RTT stray'dir ve race sınıfını korur.

   **Elenen alternatifler:** (i) geçerli girdiye hız limiti (token
   bucket / pencere) — paralel mekanizma olurdu ve bir *sayı* seçmeyi
   gerektirir (saniyede kaç aksiyon meşru? oynanış parametresi); ayrıca
   bütçeyi aşan girdi zaten göndericinin kendi kanalında taşıp **kendi**
   girdisini düşürüyor, sayılıp ona atfedilerek — hasar kendine dönük ve
   ölçülü; (ii) post-auth heartbeat'i kısmak — tek gerçek 1:1
   amplifikasyon bu, ama istemciye görünen semantiği değiştirir (RTT)
   ve `e2e.rs::active_heartbeat_survives`'a değer: ürün kararı olarak
   açık bırakıldı; (iii) bozuk MOVE_TO payload'ını bütçelemek — SECURITY
   §3'ün "dürüst-ama-hatalı istemciyi zorla düşürme" gerekçesiyle
   elendi (opcode tanımsızlığı niyet belirtir, payload bozukluğu
   belirtmez).

   **Harness düzeltmesi (test gevşetmesi DEĞİL):** `violation.rs` aktörü
   çıplak `base_table()` ile kuruyordu — hiçbir mesaj kaydetmeyen bir
   oyunun tablosu, ki `build_table()` böyle bir şey üretmez. O tabloda
   HER game-band opcode tanımsızdır, yani stray-frame testi race
   sınıfını test ediyor **gibi görünüyordu**. Harness artık gerçek bir
   tablo gibi bir game opcode'u kaydediyor; testin assertion'ları (kod
   6, ağırlık 1, 15 < 16, 16'ncıda kapanış) aynen duruyor.

   Kilit: `undefined_game_band_opcode_is_a_hard_violation` ([1,1,1,9] +
   teardown) ve tersi `registered_game_band_opcode_keeps_its_race_class`
   (tanımlı op'un dört stray'i kod 6 alıyor ve bağlantı yaşıyor —
   tanımsız bir op o sayıda kapatırdı). Mutation check: `is_registered`
   kolunu düşürmek birincisini kırıyor, ikincisi geçmeye devam ediyor.

### Doğrulama

`cargo fmt --all --check` temiz · `cargo clippy --workspace
--all-targets -- -D warnings` 0 uyarı · `cargo test --workspace`
**319 passed / 0 failed / 1 ignored** (gsb-lint doctest) · loadgen
`50 --duration 3` → `left=50 errors=0`, panik yok.

### Bu turda YAPILMAYANLAR

- Tıkanmış yazmaya süre sınırı (ürün kararı: eşik sayısı).
- AFK/zombi oturum politikası (ürün kararı; `active_heartbeat_survives`
  kilidini bilerek değiştirmeyi gerektirir).
- Geçerli girdiye hacim limiti (ürün kararı: oynanış parametresi).
- Post-auth HEARTBEAT_ACK kısması (ürün kararı: istemci RTT semantiği).
- `step_fine_hist`'in shard tarafında yazılmaması (ayrı borç, rapor
  edildi).
- `SO_KEEPALIVE` (yeni bağımlılık, ayrı savunma hattı).

## Kapatılanlar (protokol sertleştirme turu)

Kaynak: dış incelemenin protokol katmanında bulduğu üç zayıflık (a-c) +
sürüm sorusu (d) + ROADMAP P3'te bekleyen iki küçük borç (e-f). Kapsam
**sahiplik, isimlendirme ve denetlenebilirlik**; davranış değişmedi ve
**tel değişmedi** (aşağıda kanıt). Test sayısı 294 → **314** (+20; hiçbir
test silinmedi, gevşetilmedi, `#[ignore]` eklenmedi).

### Kommitler

1. **`5e21a09` — Gömülü protoc build script'lere bağlandı.**
   `gsb-protocol` `protoc-bin-vendored`'ı build-dep olarak bildiriyor ve
   panik mesajı "using vendored protoc" diyordu, ama hiçbir yere
   bağlanmamıştı: `PROTOC=/nonexistent/protoc` ile build "Could not find
   `protoc`" ile düşüyordu (yayın paketi turunun açık bıraktığı bulgu).
   `gsb-game` crate'i bağımlılığı bildirmiyordu bile. İkisi de artık
   `protoc_bin_vendored::protoc_bin_path()`'i
   `Config::protoc_executable` ile veriyor — AÇIK yol olduğu için
   `PROTOC`/`PATH` aramasının önüne geçer, yani yanlış bir `PROTOC`
   build'i kıramaz. Gömülü ikilinin olmadığı hedefte `cargo:warning` +
   eski davranışa düşüş.
   **Elenen alternatif:** bağımlılığı ve mesajı kaldırıp sistem
   `protoc`'unu şart koşmak — daha küçük diff, ama klonlanmak için var
   olan bir base'de gereksiz bir ön koşul; bağımlılık zaten lockfile'da
   ve bildirilen niyet açıkça gömülü yoldu.
   **Davranış kilidi CI'ın kendisi:** iki iş de artık
   `protobuf-compiler` KURMUYOR, yani bağlamanın bozulması CI'ı build
   script'te düşürür — hiçbir in-process testin yapamayacağı bir kapı.
   Doğrulama: `PROTOC=/nonexistent/protoc` ile build → Finished; `PROTOC`
   unset + PATH yalnızca cargo/rustc/cc'ye indirgenmiş (protoc hiçbir
   yerde) → Finished.
2. **`5f37c85` — "Reconnect uygulanmadı" iddiası düzeltildi.** README
   `docs/RECONNECT.md`'yi "**uygulanmadı**" diye listeliyordu ve
   dokümanın kendi durum satırı "TASARIM (uygulanmadı)" diyordu. İkisi de
   bayattı: ROADMAP P1 `[x]`, `crates/gsb-core/tests/reconnect.rs` 16
   test + `loadgen_smoke.rs::loadgen_churn_smoke` (§14.5 churn). İki
   satır da düzeltildi ve kilidi tutan testleri adıyla anıyor.
3. **`3bab835` — RPC yanıt zarfı base protokole taşındı.** Zarfın istek
   yarısı (`RpcRequest`) base, yanıt yarısı (`RpcResponse`) oyun
   protokolündeydi; oysa iki yarının da kuralı core'un
   (`gsb_core::rpc`: id uzayı, duplike/timeout, tam-bir-cevap
   değişmezi). İkinci bir oyun crate'i yanıt zarfını yeniden icat etmek
   zorunda kalırdı. `RpcResponse` artık `base.proto`'da; `game.proto`
   `import "base.proto"` ediyor ve `Private.responses` alanı
   `gsb.base.RpcResponse` tipinde. `gsb_core::rpc` bir
   `From<&RpcReply> for base::RpcResponse` kazandı: alan eşlemesi ve
   proto3'ün dayattığı `u16 → u32` genişletmesi core'da, oyun başına
   değil. Build: `extern_path(".gsb.base", "::gsb_protocol::base")` —
   `gsb.base` tipleri ikinci kez ÜRETİLMİYOR; `base.proto`'nun dizini
   `links = "gsb-base-proto"` üzerinden `DEP_GSB_BASE_PROTO_DIR` olarak
   taşınıyor. Karar ve dört elenen alternatif: `docs/DESIGN.md` §5.2.
4. **`00de2b3` — Gerçekten kaldırılmış tek alan numarası rezerve
   edildi.** İki `.proto`'nun tüm geçmişi tarandı. `base.proto`'dan
   **hiç alan kaldırılmamış** (her commit yalnız eklemiş) — rezerve
   edilecek bir şey yok, ve spekülatif aralık yazılmadı; denetimin
   sonucu dosyanın başına not edildi. `game.proto`'da tek kaldırma:
   `EntityState.version = 4` (2ac28d2), rolünü devralan
   `EntityRecord`'da artık `reserved 4;` + `reserved "version";`. Kapı
   derleme zamanında (protoc: `Field "version" uses reserved number 4`).
   Opcode uzayında `reserved` yok, bu yüzden emekli 1001/1002
   `gsb_game::op::RETIRED`'da ve bir test onları `MessageTable`'ın
   dışında tutuyor. Kural: `docs/DESIGN.md` §5.3.
5. **`f878ad7` — ERROR kodları yorum tablosundan üretilen enum'a.**
   `gsb.base.ErrorCode`; `Error.code` alanının tipi `uint32` →
   `ErrorCode`. Rust eşlemeleri (`ProtoError::wire_code`,
   `CoreError::wire_code`) **tüketici match** — yeni bir varyant testi
   değil DERLEMEYİ kırar (mutasyonla doğrulandı). `CoreError`'ın eskiden
   `_ => 4`'e düşen on varyantı tek tek sayılıyor: davranış aynı,
   gözden kaçma imkânı yok. `base::Error::new` sunucu tarafında hata
   üretmenin tek yolu. Loadgen'in iki istemci yolu ve örnek istemci de
   artık üretilen enum'la eşleşiyor (elle kopyalanan sayılar gitti).
   Sıfır değer ve ileri uyumluluk kuralı: §5.4.
6. **`9fe6beb` — Protokol sürümü AUTH yoluna eklendi.**
   `Auth.protocol_version` (alan 3), `gsb_protocol::PROTOCOL_VERSION`
   (= 1), `ERROR_CODE_PROTOCOL_VERSION = 13`. Ek round trip / opcode /
   frame / durum yok ve **yeni koruma makinesi yok** (uyumsuz eş zaten
   pre-auth frame bütçesi ve idle timeout ile sınırlı). `0` = sürümsüz,
   kabul. Karar, gerekçe ve altı elenen alternatif: §5.5 — koda GİRMEDEN
   ÖNCE yazıldı (doküman geleneği).

### Turun beklenmedik bulguları

- **Sürüm sorusu (d) teorik değildi.** Bu depoda tel iki kez kırıldı ve
  ikisinde de sunucunun karşıdakinin hangi wire'ı konuştuğunu anlama
  yolu yoktu: `0888441` (`sfixed32` → `sint32`, aynı alan numarası
  FARKLI wire type) ve `2ac28d2` (opcode 1003 `ENTITY_STATE`'ten
  `WORLD_SNAPSHOT`'a). Bu, ertelememe kararının ana gerekçesi oldu;
  ikincisi ise alanın **geciktikçe değersizleşmesi** (bugün "0
  gönderen" tek bir sınıf: bu commit'ten önceki istemciler).
- **`the_code_space_is_contiguous_from_zero` testi kendi turunda
  çalıştı.** (c)'de yazılan "kod uzayı bitişik" testi, (d)'de kod 13
  eklenince ÖNCE kırıldı ve sabitlenmiş listeyi genişletmeye zorladı —
  tam olarak tasarlandığı davranış.
- **`2ac28d2` opcode 1003'ü zaten geri dönüştürmüştü.** (b)'nin
  engellemeye çalıştığı şeyin bu depodaki gerçek örneği; geri alınamaz,
  bu yüzden `game.proto` başına "tarih, emsal değil" notu olarak yazıldı.

### Tel değişmediğinin kanıtı

- `.proto` diff'inde **hiçbir mevcut alan numarası değişmedi**
  (`git diff 5e6531a..HEAD -- crates/*/proto`): `RpcResponse`'un 1..5'i
  `game.proto`'dan silinip `base.proto`'ya aynen eklendi,
  `Private.responses = 3` yalnız tip adıyla nitelendi, `Error.code = 1`
  yalnız tip değiştirdi (varint → varint), `Auth.protocol_version = 3`
  tamamen yeni.
- `crates/gsb-game/tests/wire_contract.rs` — beklenen baytlar taşımadan
  ÖNCEKİ ağaçtan alındı, taşımadan sonra aynen geçiyor. Mutasyon:
  `bytes payload = 5` → `= 6` yapıldığında iki bayt testi de kırıldı.
- `error_code::the_code_field_encodes_exactly_as_the_old_uint32_did` —
  0..=13 için tek tek `[0x08, n]` (0 hiç kodlanmıyor, eski `uint32` gibi).
- `protocol_version::the_new_field_costs_nothing_on_the_wire_when_unset`
  — set edilmemiş alan hiç bayt üretmiyor (`[0x0A,0x03,'n','e','o']`).
- Gerçek soket üzerinden çerçeve çözen e2e/TLS/multi-listener süitleri
  `code == 8/9/10` iddialarıyla dokunulmadan geçiyor.

### Doğrulama

- `cargo fmt --all --check`: temiz (exit 0).
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0,
  0 uyarı.
- `cargo test --workspace`: **314 passed, 0 failed, 1 ignored**
  (gsb-lint doctest, önceden de öyle).
- loadgen 50 istemci × 3 sn (release): `left=50 errors=0 dropped=0
  tick_hz_med=30.00 server_hz=30.01 peak_conns=50`, panik yok.
- Yukarıdakiler crate'lerin `lib.rs`'lerine satır eklenip önbellek
  gerçekten kirletilerek koşuldu, sonra `git checkout` ile geri alındı.

## Kapatılanlar (yayın paketi turu)

Kaynak: `docs/HANDOFF.md` iş sırası madde 1. Kapsam: 207 dosyalık ağacın
elle koşulan `cargo test` disiplinine bağlı kalmaması için yayın
hijyeni — LICENSE, CI, MSRV, format kapısı. Davranış değişmedi (test
sayısı 294 → **294**).

### Kommitler

1. **Yedek glob import'u kaldırıldı** (`gsb-net/src/udp/client/io.rs`).
   Beklenmeyen bulgu: dosya hem `crate::udp::*` hem `super::*` import
   ediyordu; ata (`udp/client.rs`) zaten `crate::udp::*`'ı glob'luyor,
   yani ikincisi birincinin tüm isimlerini taşıyor. rustc örtüşen
   glob'larda "kullanıldı" kredisini KAYNAK SIRASINA göre veriyor:
   bugünkü sırada uyarı yok, ama `cargo fmt` `super::*`'ı öne alınca
   `unused_imports` `clippy -D warnings`'i kırıyordu. fmt kommitinin
   saf kalması için ayrı, önden kommit.
2. **`cargo fmt --all`** — 201 dosya, yalnız format. `rustfmt.toml`
   EKLENMEDİ: varsayılanlar (stil edition'ı crate'lerin `edition =
   "2024"`'ünden) her dosyayı hatasız formatlıyor; varsayılan-dışı bir
   seçeneğe somut ihtiyaç yok. Elenen alternatif: `reorder_imports =
   false` — tek bir yedek import yüzünden tüm ağacın import stilini
   varsayılandan ayırmak olurdu. rustfmt'nin kıramadığı 100 kolon üstü
   satırlar (log/format makrolarındaki uzun string literal'ler, satır
   sonu yorumları) elle yeniden yazılmadı.
3. **LICENSE + MSRV + CI + CONTRIBUTING.**
   - MIT (2026, furkangkhsn); yedi crate'in hepsi zaten
     `license.workspace = true`.
   - `rust-version = "1.95.0"` (yedi crate `rust-version.workspace =
     true` ile miras alır) ve `rust-toolchain.toml` `stable` →
     **`1.95.0`**. MSRV'nin alt sınırı bağımlılıktan geliyor: lockfile'daki
     en yüksek `rust-version` `bevy_ecs 0.19.1` = 1.95.0; rustc 1.94.1
     build'i "bevy_ecs@0.19.1 requires rustc 1.95.0" ile reddediyor.
     1.94'ün altı denenmedi — bu lockfile ile kendi kodumuzdan bağımsız
     olarak derlenemez. MSRV = sabit toolchain olduğundan CI'da ayrı
     MSRV işi yok.
   - `repository = "https://example.local/..."` yer tutucusu
     KALDIRILDI: git remote yok, hiçbir crate alanı miras almıyordu;
     uydurma URL yazmak alanın yokluğundan kötü.
   - `.github/workflows/ci.yml`: ubuntu-24.04 + 1.95.0 üzerinde üç iş —
     `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
     -- -D warnings`, `cargo test --workspace`. gsb-lint taraması her
     build script'te koştuğu için yasak desen kapısı clippy/test
     işlerinin içinde. Elenen alternatif: ayrı bir lint işi — aynı
     taramayı üçüncü kez derlemek olurdu.
   - `CONTRIBUTING.md`: HANDOFF/README'deki bağlayıcı disiplinin özeti
     (yeni kural eklenmedi).

### Turun bulguları (açık bırakıldı — ROADMAP P3)

- **Build sistem `protoc`'una bağlı.** `gsb-protocol` build-dep olarak
  `protoc-bin-vendored` bildiriyor ve `build.rs`'in panik mesajı "using
  vendored protoc" diyor, ama crate hiçbir yerde bağlanmamış:
  `PROTOC=/nonexistent/protoc` ile build "Could not find `protoc`" ile
  düşüyor. CI bu yüzden `protobuf-compiler` kuruyor.
- **`loadgen/churn.rs` log metninde gömülü boşluk blokları**: "join
  answered 'stale … resume' … same … connection" string literal'i iki
  yerde 38'er boşluk taşıyor (eski bir satır birleştirmenin izi).
  rustfmt literal'e dokunmaz; format turunda düzeltilmedi.

### CI'da kırılganlık riski (test silinmedi / `#[ignore]` eklenmedi)

Paylaşımlı runner'larda duvar saatine bakan süitler: `loadgen_smoke`
(debug build'de 20-40 Hz aralığı, ≥60 adım), frame-independence,
idle/heartbeat timeout'ları, READ adaleti ve supervision testleri.
Yerel makinede (16C/32T) hepsi yeşil; runner'da oynaklık görülürse
önce veri (hangi test, hangi eşik), sonra karar.

### Doğrulama

- `cargo fmt --all --check`: temiz. `cargo clippy --workspace
  --all-targets -- -D warnings`: 0 uyarı. `cargo test --workspace`:
  294 passed, 0 failed (1 ignored: gsb-lint doctest, önceden de öyle).
- loadgen 50 istemci × 3 sn (release): left=50, errors=0, dropped=0,
  tick_hz_med=30.01, server_hz=29.97, panik yok.
- `ci.yml` PyYAML ile ayrıştırıldı; her `run` komutu yerelde aynen
  koşuldu (GitHub Actions'ın kendisi burada koşulamadı).

## Kapatılanlar (çoklu-listener'a QUIC + WS kapıları turu)

Kaynak: üç ayrı oturumda erken sonlanan sözleşmeli tur (ROADMAP devam
notu). Kapsam: çoklu-listener maddesinin kalan paragrafı — hazır olan
`QuicTransport` (`gsb-net/src/quic.rs`) ve WS listener'ının
(`gsb-net/src/ws.rs`) `[[listeners]]` tablosuna takılması. Aktör/oda
katmanına dokunulmadı: iki yeni kapı da mevcut `bind_listener` →
`run_accept` → `AcceptPipeline` yoluna girer; tek ConnectionId sayacı,
tek registry, transport-bağımsız odalar değişmedi.

### Kararlar

- **"quic" girdisi tls_cert/tls_key'i YENİDEN KULLANIR** (iki yeni alan
  yok): QUIC altında TLS 1.3 ZORUNLUDUR; "tls" kapısıyla aynı PEM
  çiftini yüklemek "bir kimlik, iki kapı" demektir. Elenen alternatif:
  ayrı `quic_cert`/`quic_key` anahtarları — aynı dosyaların kopya
  yazımı + iki kapının sessizce farklı kimliğe kayma riski; hiçbir
  kullanım senaryosu göstermedi.
- **WS mesaj tavanı türetilir, knob eklenmez**:
  `max_frame_bytes + 4` (WS mesajı = 4 baytlık uzunluk öneki + frame
  gövdesi; gövde tavanı tüm kapılarda ortak). Elenen alternatif: ayrı
  `ws_max_message_bytes` — diğer kapıların yasakladığı bir frame'in WS
  kapısından geçmesi ya da tersi, tek aktör yığını politikasını kapı
  başına fork ederdi (ListenerEntry dokümanındaki global-knob ilkesi).
- **"quic"/"ws" legacy skaler gramer'e SOKULMADI** (yalnız dizi
  yazımı): `TransportKind`'in genişliği sabit kalır; eski bir config
  yeni parser altında anlam değiştiremez ("tls" için kurulan aynı
  gramer-genişliği argümanı).
- **wss bu gramarde ifade edilmiyor**: `transport = "ws"` düz TCP üstü
  RFC 6455 upgrade'idir; TLS dosyası taşıyan "ws" girdisi startup'ta
  `ListenerWsWithTls` ile reddedilir (sessiz yeniden yorumlama yok).
  Gerçek ihtiyaç doğduğunda "wss" ayrı yazım olarak eklenir.

### Turun yakaladığı hata (düzeltildi + kilitlendi)

Mevcut TLS-array doğrulama kolu iki error variant'ını kendi metinlerine
göre TERS kullanıyordu (`cert`'siz girdi "with tls_cert but no tls_key"
mesajı veriyordu; legacy skaler yol doğruydu, dizi yolu değildi).
Hiçbir test yönünü kilitlemiyordu. Düzeltme state-descriptive eşlemeyle
yapıldı ve hem "tls" hem "quic" kolları için yarım-dosya testleri
(`tls_entry_without_both_tls_files_refuses_startup`,
`quic_entry_without_both_tls_files_refuses_startup`) yönü mühürledi.

### Doğrulama

- İki karışık-transport e2e: `quic_and_tcp_doors_serve_one_room`
  (gerçek quinn istemcisi — mini-PKI'ya güvenir, ALPN `gsb-net/1`, tek
  bi-stream — düz-TCP istemciyle aynı odada karşılıklı görünürlük) ve
  `websocket_and_rudp_doors_serve_one_room` (ham-TCP RFC 6455 istemci —
  gerçek upgrade handshake, accept-key RFC 6455 §1.3 vektörüyle
  doğrulanır, maskeli binary mesaj başına tek game frame — rUDP
  istemciyle aynı odada karşılıklı görünürlük).
- Config kilitlemeleri: yukarıdaki yarım-dosya ikilisi +
  `ws_entry_with_tls_files_refuses_startup`.
- Süit: 287 → **292** (multi_listener 6 → 11). clippy --workspace
  --all-targets: 0 uyarı. loadgen 50 istemci × 3 sn: left=50,
  errors=0, dropped=0, tick_hz_med=30.01.
- Bağımlılık notu: gsb-server'a yalnız DEV-dep olarak `quinn` eklendi
  (sunucunun konuştuğu crate'in gerçek istemcisi; release derlemesine
  girmez).

## Kapatılanlar (regresyon ölçüm turu)

Kaynak: reconnect → trait birleşimi → shard-RPC → ops → güvenlik
turlarının ardından hiçbir büyük yük ölçümü tekrar alınmamıştı;
proje ilkesi "önce veri" gereği C1 tablosunun bugünkü kodla tekrarı.
Hiçbir kod değişmedi — bu tur salt ölçüm (+ bir harness düzeltmesi).

### Ana tablo: C1 aynası (orchestrator, --pin --procs 4, spatial c5,
ring, 30 sn; baseline = koruma katmanı turu kayıtları)

| Ölçek | Metrik | Eski | Bugün | |
|---|---|---|---|---|
| 5k | step p50 / hz | 12.5 ms / 30.00 | **12.5 ms / 29.99** | = |
| 9k | hz / over_budget | 28.19 / %44.6 | **30.00 / %13.7** | daha iyi |
| 10k | step p50 / hz | ≥50 ms / 23.21 | **25 ms / 29.37** | **daha iyi** |
| 10k | drops / late_max | 52 771 / 2.17 sn | 24 646 / 2.14 sn | daha iyi |
| 10k sharded N=4 | server CPU | 111.4 çekirdek-sn | **61.5 (−%45)** | daha iyi |

**Sonuç: regresyon yok — duvar geriledi.** 9k-10k arasındaki bütçe
aşımı kayboldu (p50 yarıya indi, hz 23.2→29.37); delta yayın ve sonraki
turların kazancı tüm eklenen makinenin (resume bağlaması, RPC muhasebesi,
detach süpürgesi, PlayerId) bedelini fazlasıyla ödüyor. Sharded N=4'te
CPU maliyeti ~%45 düştü. Milyonluk `dropped` sayısı aynı bilinen
istemci-decode artefaktıdır (sunucu duvarı değil).

### Yan bulgular

- **In-proc worker düzeltmesi:** varsayılan `--workers 0` TEK tokio
  worker anlamına geliyordu; 1000 istemcide oda actor'ü tick'ler
  arasında açlıktaydı (19-22 Hz, sub-ms adımlar, ~200 ms late_max).
  Düzeltme: workers=0 artık available_parallelism demek (açık
  --workers kazanmaya devam). Doğrulama: 1000 istemcide 30.00 Hz,
  drop 0; --workers 8 ile p50 782 µs (eski ölçümde 3 000 µs — 4×).
- **TLS fiyat noktası (ilk ölçüm):** 500 dış istemci, rustls:
  istemci tarafında ~%8-11 verim bedeli, sunucu CPU'sunda fark yok.
- **Churn yük altında:** 200 istemci × 2 döngü → 400/400 resume,
  0 stale-reject, 0 fresh-join, 30.01 Hz, drop 0 (RECONNECT §14.5
  ölçüm planının ölçekli koşusu).

Ham RESULT satırları `.loadrun-logs/` altında tur etiketleriyle
saklıdır (S0-S9). Ortam: 7950X 16C/32T, release, loadavg 5-11/32.

- [x] **Çoklu-listener: aynı haritada karışık transport istemcileri** —
  Uygulandı ✅ (`[[listeners]]` dizisi; ConnectionId merkezi sayaç;
  geriye-dönük uyum; 6 yeni e2e kilidi dahil üç transport tek odada
  doğrulandı). Kalan: QUIC/WS listener'larının bu listeye takılması.
  config tek transport yerine listener LISTESİ alsın (`[[listener]]`
  transport+bind çiftleri); her listener kendi accept görevini koşturur,
  her kabul standart bağlantı aktörünü doğurur (ConnectionId merkezi
  sayaçtan). Aktör/oda katmanına dokunmaz — odalar transport-bağımsızdır,
  karışık istemci (örn. aynı odada QUIC + TLS-TCP + rUDP) mimaride zaten
  serbesttir. Küçük-orta iş.
- [x] **QUIC taşıması** (quinn) — connection-migration + 0-RTT:
  rehome protokolünün tercih edilen taşıyıcısı. Uygulandı: tek bi-stream
  + length-prefix = TCP semantiği; ALPN gsb-net/1; 10 sn handshake
  tavanı; rustls/ring provider tls.rs ile aynı. ✅
- [x] **WebSocket taşıması** — elle RFC 6455: upgrade handshake
  (sha1+base64, RFC vektörü testli), maskeli-client zorlaması,
  ping/pong/close, fragmentasyon; her WS binary mesajı bir length-
  prefixed game frame taşır (tcp.rs ile validation simetrisi). ✅
  *(İki implementasyon da Transport trait'inde — aktör katmanı dokunulmadı;
  kalan parça: server-config çoklu-listener bağlaması — aşağıdaki madde.)*


## Kapatılanlar (güvenlik turu)

Kaynak: dış inceleme ailesinin kalan teknik maddeleri — şifreleme,
auth rate-limit, pre-auth tahsis sınırı. Tasarım: `docs/SECURITY.md`.

- **TLS (Tur A):** `TlsTransport` (rustls, ring provider) — PEM yükleme
  bind'ta (hatalı dosya adıyla hata), handshake 10 sn tavanı; pump
  katmanı generic FrameReader/FrameWriter'a çıkarıldı (tcp byte-identical
  alias, tls aynı adaptörleri besler). Config: `tls_cert/tls_key`;
  tek-taraf ve udp+tls başlatma hatası — sessiz zayıf geri düşüş reddi.
  Tüm guardrail e2e'leri tcp+udp+TLS üçlüsünde koşar; test sertifikaları
  rcgen ile koşu-anında üretilir (dev-dep; repoya sertifika kommitlemez).
- **Rate-limit + cap'ler (Tur B):** AUTH penceresi (3/10 sn; aşım HARD
  ihlal), pre-auth heartbeat yanıtı 1/sn (fazlası sessiz sayaç — bütçe
  DEĞİL: gerekçesi kod içi), pre-auth 64 frame bütçesi (ERROR 9),
  registry'de unauthed oturum cap'i (`max_connections/4`, taban 64,
  `0` kapatır; resume authed sayılır).
- **Ortam notu:** yeni bağımlılıklar HOME cargo önbelleğinin salt-okunur
  olduğu ortamlarda CARGO_HOME=$PWD/.cargo ile workspace-vendor'dan
  çözülür.

### Testler

- `tls_e2e.rs` (6): tam akış, yanlış-CA reddi (iki uç), tek-taraf config
  hataları, udp+tls reddi, plaintext-default koruması; gsb-net 4 unit
- `security.rs` (6): auth flood→bütçe→kapanma, pencere davranışı,
  heartbeat fırtınası sessiz sayımı, 64-frame kapanması, unauthed cap,
  resume-authed sayımı

Test 215 → **231** (231/231 yeşil); clippy temiz; loadgen regresyonu yeşil.


## Kapatılanlar (ops yüzeyi turu)

Kaynak: dış inceleme sonrası yol haritasının 2 numaralı maddesi — sunucu
uzun ömürlü durum tutarken (park edilmiş oyuncu, pending RPC) gözlemlenemez
olması. Tasarım: `docs/OPS.md`.

- **Sıfır yeni bağımlılık:** HTTP/1.1 elle yazıldı (tokio TcpListener;
  yalnız GET + `Connection: close`). Elenenler: axum/hyper (bağımlılık
  bütçesi), JSON parser (admin query-string ile yeterli).
- **Metrik akışı:** `MetricSink::Watch(watch::Sender<MetricReport>)` —
  collector her periyotta en son raporu watch'a iter, HTTP task `borrow()`
  ile okur (tek-yazar/kilitsiz). `render_prometheus()` ayrı fonksiyon:
  `gsb_*` adlandırma, `*_total` sayaç sözleşmesi, histogram bucket'ları
  gerçek µs kenarlarla.
- **Endpoint'ler:** `/healthz` (liveness: rapor yaşı ≤ 3 periyot),
  `/metrics` (text 0.0.4), `/rooms` listeleme + `/rooms/open|close`
  (mevcut ServerHandle komutları — yeni kontrol yolu yok).
- **Güvenlik duruşu:** varsayılan kapalı (`http_listen = ""`);
  localhost-only sözleşme; auth NOT-DONE olarak beyazlı.
- **Bulgular:** (1) ilk ajanın prometheus render'ında `+Inf` kovası
  toplam gözlem sayısını taşımıyor ve assertion'larda düz-string içinde
  `{{}}` hatası vardı — fixture üretim değişmeziyle (`sum(step_hist)==steps`)
  tutarlı hale getirildi, assertion'lar hesaplanan kova indekslerinden
  türetildi; (2) `emitted_at` alanı loadgen codec'ini kırmıştı (GSM1
  formatı korundu).

### Testler

- `http_ops.rs :: healthz_answers_503_before_the_first_report`,
  `healthz_reports_ok_while_ticker_runs`,
  `metrics_endpoint_exposes_known_counters`,
  `admin_open_status_close_round_trip`,
  `unknown_path_wrong_verb_and_bad_params_are_rejected`,
  `disabled_by_default_and_no_listener_without_config`
- `metrics.rs :: render_prometheus` unit süiti (aile başlıkları,
  `*_total` sözleşmesi, histogram tutarlılığı, boş-rapor davranışı)

Test 205 → **215** (215/215 yeşil); clippy temiz; loadgen regresyonu yeşil.


## Kapatılanlar (tablo budama turu)

Kaynak: dış inceleme raporunun 3 numaralı bulgusu — shard actor'lerinin
`conn_epoch` ve `conn_tombstone` tabloları bağlantı churn'üyle **sonsuz
büyüyordu** (leave/close'ta hiç silinmiyorlardı). Uzun ömürlü süreç hedefi
için tam da hedef senaryoyu vuran sızıntı: giriş başına ~80 B × iki tablo ×
milyonlarca tarihsel bağlantı = haftalarla ölçeklenen ölü durum. Aynı
inceleme ailesinden `MetricAccumulator.rooms` (yıkılan her odanın
biriktiricisi sonsuza kadar tutuluyordu) ve `conn_actions_dropped`
(ConnectionId bazlı, hiç budanmayan) aynı kapsamda çözüldü.

### Tasarım kararları (dış danışma ile birlikte verildi)

- **`conn_epoch`: Leave'te anında silme.** Tek okuma noktası (:730)
  yalnızca bu shard'ın tablosunda CANLI olan bağlantının giden Migrate
  damgası; leave sonrası canlı olamaz ve gelecekteki migrate-in yeniden
  ekler. Yarış penceresi yok — saf kazanım.
- **`conn_tombstone`: tick-damgalı tembel süre (seçenek B).** Anında silme
  reddedildi: tombstone'un VARLIK SEEBEBİ leave'ten sonra gelebilecek
  gecikmiş Migrate'i reddetmek (hayalet-dirilme yarışı). Dispatcher-driven
  Forget de reddedildi: Migrate komşudan, Forget registry'den gelir —
  farklı göndericilerin tek FIFO kuyruğundaki işlenme sırası global
  gönderim sırasını garanti etmez; doğruluk argümanı lokal olmaktan çıkar.
  Seçilen şekil: değerler `(epoch, write_tick)`; CONTROL fazı her 512
  tick'te bir (`TOMBSTONE_SWEEP_EVERY_TICKS`) yaşları 256 tick'i
  (`TOMBSTONE_TTL_TICKS`) aşan girişleri tek `retain()` ile atar.
  Doğruluk argümanı (kod içi belge): TTL'den eski bir Migrate'in kuyrukta
  yaşamış olması için shard'in sağlıklı çalışma noktasının çok ötesinde
  takılmış olması gerekir — bounded kanallar her tick drene edilir;
  mevcut bozulma notları (1-tick hizalama, sınırlı blink) zaten aynı
  varsayıma dayanır. Sabitler config'e ÇIKARILMADI: doğruluk
  parametresidir, operatör ayarı değildir (dış kararla sabitlendi).
- **Metrik budaması:** yıkılan odaya `MetricsEvent::RoomGone` bildirimi →
  biriktirici İKİ rapor penceresi "donuk ama süpürgeye kapalı" bekletilir,
  sonra düşer (ölüm, son örnek ile sonraki rapor arasına denk gelirse son
  ölçülen pencere kaybolmasın diye — anında silmek tam da bunu yiyordu).
  Shutdown-all bilerek bildirim üretmez (her odanın final rapor penceresi
  korunur). Kapanan bağlantının `actions_dropped` girişi son flush'ında
  emekli edilir; toplam skalar biriktirilir, `net.actions_dropped`
  monoton kalır, `actions_dropped_top` yalnız canlı bağlantıları sayar.
- **`RoomConfig::period` totalliği:** imza korundu (6 çağrı noktasına
  dalgasız); `Ticker::spawn`'daki guard'ın aynısı ile geçersiz hızda
  belgelenmiş 1 sn yedek periyot. Registry yolu böyle configleri zaten
  reddeder (doğrulanıp comment'e yazıldı); panik yolu yalnız el-yapımı
  config'teydi.

### Turun asıl bulgusu: roster-drift panigi (supervision turunun yakaladığı)

Budama, hayalet-biriktirici davranışını kaldırınca görünür oldu: loadgen'in
son-of-run toplu çıkışında **oda task'i HER koşuda panikliyordu** — eski
maskede ölü odalar raporlamaya devam ettiği için smoke yeşil kalıyordu.
Kök neden klasik bir `swap_remove` dönüş-değeri yanılgısıydı:
`roster_remove`, pozisyon düzeltmesini **silinen** elemanın (return
değeri!) girişine yapıyordu; idx'e taşınan elemanın girişi bayat kalıyor,
ilk kuyruk-dışı leave'te sürüklenme başlıyordu. Supervision turunun ölüm-
hasatı tam da bu sessiz task ölümünü görünür warn + reaped room'a
çevirdi — katmanın kendisi kendi sonraki böceğini yakaladı. Düzeltme:
düzeltme RELOKATE edilen elemana (`roster.last()`), insert ile; regresyon
kilidi `room.rs :: roster_stays_synchronized_through_mass_leaves`
(50 join → hepsinin leave'i → üç yapı da boş + sonrası join çalışır).

### Testler

- `shard.rs :: leave_prunes_the_epoch_entry`,
  `stale_migrate_rejected_then_tombstone_expires` (mevcut reddetme
  davranışı korunumu dahil)
- `metrics.rs :: destroyed_room_accumulator_is_pruned_and_stragglers_suppressed`,
  `closing_connection_retires_its_actions_dropped_entry`
- `room.rs :: period_is_total_for_hand_built_configs`,
  `roster_stays_synchronized_through_mass_leaves`

Test 166 → **172** (172/172 yeşil, iki ardışık tam koşu);
`cargo clippy --workspace --all-targets` temiz; `gsb-loadgen 50 --duration 3`
artık paniksiz (`left=50`, önceki her koşuda panic).

### Kalan (bu inceleme ailesinden)

(1) çerez rotasyonu + pre-auth tahsis sınırı (güvenlik turu); (2) rUDP REL
give-up — ERTELENDİ (deneysel statü); (3) reconnect/reattach seam'i
(ROADMAP P1'deki oturum politikası maddesiyle birleşir).


## Kapatılanlar (supervision turu)

Kaynak: dış inceleme raporunun 2 numaralı bulgusu — `logic.update()`
içindeki bir panik oda task'ini sessizce öldürür ama registry tablosundaki
kayıt yaşamaya devam eder: durum ebediyen `Running{members}` görünür,
join'ler ölü kontrol kanalına dispatch edilir. 161 testin hiçbiri bu yolu
kapsamıyordu (panik eden oyun mantığı hiç simüle edilmemişti). Sharded
odada tek shard'ın ölümü de mantıksal odayı kırar: komşular ölü shard'ın
kapalı mailbox'ına sender tutar, migrasyon asla tamamlanamaz.

### Tasarım: izleyen watcher + kusursuz-giriş hilesi

- **Tespit:** her spawn edilen oda/shard task'i için TEK watcher task —
  yalnız kendi `JoinHandle`'ını bekler, ölünce yeni
  `RegistryMsg::RoomDied { id, shard, generation }`'i registry'nin
  kendi self-mailbox'ına raporlar. Aktör disiplini korunur: registry'nin
  tek await'i yine inbox `recv`; watcher'ın tek await'i handle; select yok.
  Neden polling değil: `JoinHandle::is_finished()` ile mesaj-başı tarama
  mümkündü ama tespit gecikmesi sınırsız olurdu (sessiz sunucuda zombi
  penceresi sonsuz) ve "nerede poll edileceğini hatırlama" disiplini
  gerektirirdi — projenin "yapısal işaret > yazar disiplini" ilkesinin
  tersine.
- **Normal/istisnai ayrımı iptal altyapısı olmadan:** her çıkış raporlanır;
  `DestroyRoom`/`Shutdown` tablo girişini senkron siler, dolayısıyla geç
  gelen rapor ya giri bulamaz ya da **farklı kuşağın** girişi bulur — ikisi
  de sessiz no-op. Yalnız CANLI giriş + AYNI kuşak = beklenmedik ölüm.
  Kuşak sayacı (`room_gen`, id başına) install'da artar, hem gire hem
  watcher'a damgalanır; bayat rapor tek integer karşılaştırmasıyla reddedilir.
- **Beklenmedik ölüm:** warn (oda + shard indeksi), tablodan düşme,
  üyelere `ConnIn::RoomGone` (destroy semantiğiyle birebir — ortak
  `notify_room_gone()` helper'ı), bağlılık temizliği, `rooms_died`
  metriği. Durum sorguları artık doğru cevabı verir: `Absent`.
- **Sharded odada bir shard ölürse:** kısmi toparlama YOK — komşu kanalları
  kopuk olduğu için bütün mantıksal oda hasat edilir (tek oda ölümüyle aynı
  yol), warn ölen indeksi isimlendirir.
- **Restart politikası:** `RoomConfig::restart_on_panic` (varsayılan false;
  sıradan bir `PartialEq` katılımcısı — alanı çevirmek idempotent-create'te
  doğal olarak `RoomConflict` üretir). Açıkken ölüm sonrası oda AYNI
  factory+config'den `install_room()` üzerinden yeniden kurulur ve
  **BOŞ döner** (üyeler zaten haberdar edildi, rejoin edebilir; dünya
  durumu ölü task'in içindeydi). Sürekli panikleyen logic = ölüm başına
  bir warn'lık restart döngüsü — operatöre anında görünür, v1 için kabul
  (backoff makinesi yok). Factory'nin kendisi registry döngüsünde çağrılır
  (CreateRoom'taki mevcut maruziyetin aynısı); panikleyen factory kapsam
  dışı, kod içinde beyanlı.

### Turun bonusu: iki yarış deliği kapandı

Testleri kurarken görüldü: bir join dispatch edildikten sonra oda ölürse,
geciken `SpawnDone`/`SpawnFailed` raporu yanlış enkarnasyona düşebilir
(yeni ShardGroup'un sayaçlarını bozar ya da ölü üyeliği canlı tabloya
sabitler). Çözüm: dispatch anındaki kuşak `RoomOp::Join`'e damgalanır,
dispatcher `SpawnDone`/`SpawnFailed`'da geri yankılar; bayat settle ya
sessizce atılır ya bağlantıya `RoomGone` bildirilir — hangi sıralıda
olursa olsun yakınsama garanti (sharded cap muhasebesi saturating).

### Testler (davranış kilidi; 5× tekrarda sıfır flake)

- `supervision.rs :: panicking_room_is_removed_and_members_notified` —
  ilk tick'te panic eden logic: durum `Absent`, üye `RoomGone` aldı,
  sonraki SpawnPlayer `RoomNotFound`. Determinizm kilit'siz: panik
  logic'in KENDİ join bayrağıyla kurulur (paylaşımlı atomik yok).
- `supervision.rs :: restarted_room_comes_back_when_policy_enabled` —
  yalnız ilk update'te panikleyen logic + `restart_on_panic`: oda yeniden
  `Running`, eski üye haberdar edilmiş, fabrika tam 2 kez kurulmuş
  (crash-loop yok), taze join çalışıyor.
- `supervision.rs :: shard_death_takes_down_the_whole_logical_room` —
  2 shard'lık odada shard 1 ölür → bütün oda `Absent` + üyelere `RoomGone`.

Metrik dalgası küçük tutuldu: `rooms_died` registry sayaçları +
`RegistrySample` + `RegistryReport` + render satırı + loadgen binary
wire encode/decode (~15 satır).

Test 163 → **166** (166/166 yeşil); `cargo clippy --workspace
--all-targets` temiz.

### Kalan (bu inceleme ailesinden)

(1) shard tablo budaması — `conn_epoch`/`conn_tombstone` (Migrate yarışını
korudukları için TTL ya da güvenli pencere kararı gerekli),
`MetricAccumulator.rooms`, `conn_actions_dropped`; (2) `RoomConfig::period`
panik yolu (hızlı düzeltme turu B notu); (3) çerez rotasyonu + pre-auth
tahsis sınırı (güvenlik turu); (4) rUDP REL give-up — ERTELENDİ (rUDP
deneysel statüsünde; kanıtlanmış taşıma seam'i bekliyor).


## Kapatılanlar (dış inceleme hızlı düzeltme turu)

Kaynak: bağımsız bir dış inceleme raporu (gsb-core/net/server/protocol +
dokümanlar) ve rapor iddialarının tek tek doğrulanması. Beş madde, hepsi
"bir sonraki turda canını yakar" kategorisinden; mimariye dokunmadan.

### A — Doküman çürüğü: metrik kanalı "unbounded" iddiası

Metrik altyapısı bounded kanal + senkron `try_send` + sayılan drop olarak
kurulmuştu (metrik turu), ama üç yerdeki doküman hâlâ "*unbounded* send"
diyordu: `metrics.rs` modül dokümanı (satır 6) kendi tasarım paragrafıyla
(14-23) **15 satır arayla çeliyordu**, `RoomSample` doc'u ve `room.rs`'teki
iki kardeş iddia aynı bayatlığı taşıyordu. Dokümanın spesifikasyon rolü
oynadığı bu projede çürüme kod hatasından ağır: okur ya yanlış inanır ya da
dokümana güvenmeyi bırakır. Dört nokta da gerçek tasarıma (bounded +
`try_send` + drop sayacı + kümülatif örnek) çevrildi.

### B — `Ticker::spawn` panik yolu → tipli hata

`Ticker::spawn(hz, _)` genel API; `Duration::from_secs_f64(1.0/hz)` hz ≤ 0
ve NaN'da panikliyor, aşırı küçük hızda overflow, aşırı büyük hızda sıfır
periyoda sessiz yuvarlanıyordu (sıfır periyot = tick değil busy-loop).
Registry yolu config'i zaten doğruluyordu; ama spawn'ı doğrudan çağıran
test/platform gömücüsü panikle ölüyordu. Yeni sözleşme: guard totol
(`finite && > 0 && try_from_secs_f64 → non-zero`) ve hata tipli —
`CoreError::InvalidTickRate { rate }`; kompozisyon kökü yeni
`ServerError::BadTickRate` ile net bir başlangıç hatası veriyor.
`Ticker::period()` "by construction paniksiz" olarak belgelendi (bir
`Ticker` değeri yalnızca geçerli periyot üretmiş hızla var olabilir).
Yeni test: `spawn_rejects_rates_without_a_period` (0, negatif, NaN, ±inf,
1e15). **Kalan (bilinçli):** `RoomConfig::period` el-yapımı config'te hâlâ
panikleyebilir; registry yolu böyle config'i reddettiği için düşük öncelik
— kapatılış biçimi oda-config doğrulamasının tek merkezde toplanmasıdır,
ayrı maddedir.

### C — SIGTERM graceful shutdown'a bağlandı

`main.rs` yalnız `ctrl_c()` bekliyordu; `docker stop` (SIGTERM) altında
kapanma kaskadı hiç çalışmıyordu. Projenin çoklu-kaynak idiom'uyla çözüldü
(select! yasak): sinyal başına bir watcher task, her biri bounded kanala
raporluyor; main tek `recv` bekliyor. Windows davranışı değişmedi (unix
gövdesi cfg'li).

### D — README senkronu

README kodun gerisinde kalmıştı: "38 test" (gerçek: 163), rUDP "yarın
eklenir" (kodda iki transportla ship edilmiş durumda: cookie handshake,
REL kontrol bandı / RAW oyun bandı, `transport = "udp"`), delta+AOI
"belgelenmiş sonraki adım" (`spatial` stratejisiyle ship edilmiş; team/PVS/
sharded dahil `visibility` seçimiyle). Üçü de kaynağına karşı doğrulanarak
düzenlendi; test sayısı bundan sonra ölçülerek yazılır.

### E — READ fazı açlığı (döner imleç) + adalet testi

READ, bağlantıları `conns` HashMap'inin tekrar sırasında geziyordu; sıra
koşu içinde sabit olduğundan sürekli flood altında aynı hash-sırası öneki
oda çekme bütçesini (65 536) her tick tüketiyor, kuyruk bağlantıları
**hiç** ulaşılmaz kalıyordu ("ertelendi ≠ hiç teslim edildi"). Bu, girdi
adaleti turunun kapatdığı atma-adaletinin ulaşma tarafındaki kardeşidir:
(a) bir bağlantının tick'ten alabileceği sınırlandı; artık (c) kalanın
kimden alınacağı da döner. Tasarım: `roster: Vec<ConnectionId>` (join
sırası) + `roster_pos` indeks haritası (swap-remove + tek indeks düzeltmesi,
O(1) amortize) + mutlak `read_cursor` (incelenen her bağlantının üzerinden
ilerler; üyelik değişimi imleci bozmaz, yalnız başlangıç ofsetini kaydırır).
Tick başına ek tahsis yok, sıralama yok, kilit yok; per-conn bütçe, oda
çekme bütçesi, isteklerin varış-sırasında ayrılması ve `dropped_actions ==
0` semantiği aynen korundu. Roster, `conns` tablosunun değiştiği üç kontrol-
yolunda senkronlanır (join / leave / eskitilmiş rejoin).

Test: `read_fairness.rs :: sustained_overload_reaches_every_connection_within_n_ticks`
— ilk katılan flooder'ın 64 bekleyen aksiyonu karşısında çekme bütçesi
2 × per-conn 1 iken altı bağlantının her birinin ilk ingest'i N=6 adım
içinde gelir; ≥3 ayrı tick gerekmesi bütçenin gerçekten bağlı olduğunu
kanıtlar (yarış argümanı); drop 0. **Mutation-verified:** taramayı eski
hash-sırasına döndürmek testi düşürür ("connection 2 was NEVER reached").

### Tur özeti (yeni testler, hiçbir eski test silinmedi/ihmal edilmedi)

- `ticker.rs :: spawn_rejects_rates_without_a_period` (unit)
- `tests/read_fairness.rs :: sustained_overload_reaches_every_connection_
  within_n_ticks` (entegrasyon)
- Test 161 → **163** (163/163 yeşil + 1 var olan ignored doctest);
  `cargo clippy --workspace --all-targets` temiz.

### Kalan (bu incelemeden doğan, henüz açık)

Önceliğe göre: (1) **REL bandı give-up'ı oturum ölümcül yapmak** — teslim
edilmiş özellikte doğruluk hatası: N yeniden gönderimden sonra sessizce
vazgeçiliyor, kaybolan `JOIN_ROOM_RESULT` istemciyi sonsuza kadar
bekletiyor. **Karar (bu tur): ERTELENDİ** — rUDP deneysel kabul edildi;
üretimde aynı `Transport` seam'i arkasından kanıtlanmış bir taşıma
(QUIC tabanlı) koşacak ya da bu boşluk o zaman kapatılacak. Bedeli:
`udp.rs` modül dokümanı artık "experimental" statüsünü ve sessiz give-up →
yön-kilidi mekanizmasını açıkça beyan ediyor ("reliable within the give-up
bound"); config'deki `transport = "udp"` seçeneği bu statüde. (2)
**aktör supervision'ı** — `logic.update()` panigi oda task'ini öldürür,
registry kaydı `Running` görünmeye devam eder (zombi oda); (3) **shard
tablo budaması** — `conn_epoch`/`conn_tombstone` (Migrate yarışını
korudukları için dikkatli: TTL ya da güvenli pencere kararı gerekli),
`MetricAccumulator.rooms`, `conn_actions_dropped`; (4) `RoomConfig::period`
panik yolu (B'deki not); (5) çerez rotasyonu ve pre-auth tahsis sınırı
(güvenlik turu — rUDP deneysel statüsüne bağlandı).


## Kapatılanlar (reject-bucket wiring + sayaç envanteri turu)

**Tur kapsamı.** Küçük temizlik turu; yeni mekanizma yok. (A)
**Reject-bucket wiring** — `requests_rejected_*` altı kovası koddaki altı
terminal ret kararının her birine karşılık geliyor, ama hangi retin hangi
kovaya yazıldığını doğrulayan test YOKTU: smoke testleri altısının da
sıfır olduğunu kontrol ediyor — bu, rapor kuyruğunun kaymasını yakalar,
kablolamanın yanlış olmasını yakalamaz (kova karışımı bugünde tüm testleri
geçirirdi). Bu önemli, çünkü ayırmanın tek amacı operasyonel sinyaldi:
"oda cap'i gerçekten bağlıyor mu?" Yanlış kovaya düşen ret o soruya
kendinden emin ama yanlış cevap verir. (B) **Sayaç envanteri** — A'daki
boşluk bir örnek değil, bir sınıf: doğruluğunu hiçbir testin kontrol
etmediği metrik. Mevcut sayaçlar tarandı; envanter aşağıdaki tabloda.
Bu turda yalnız ret/RPC ailesi (A'nın kapsamı) kapatıldı; kalan maddeler
P0'da adlandırılmış madde. (C) **`.measure/` hijyeni** — bir önceki turun
ölçüm ham çıktıları untracked duruyordu ve rapor "ağaç temiz" diyordu
(iki tur önce `.scratch/`'le aynı durum); `.gitignore`'a eklendi
(`.scratch/` emsalı) ve `git status --porcelain` gerçekten boş
doğrulandı. Wire protokolü değişmedi; Mutex/RwLock/parking_lot/select!
yok; `unsafe_code = "forbid"`; mevcut 154 testin hiçbiri silinmedi/
`#[ignore]`'lenmedi. Test 154 → **161**.

### A — Reject-bucket wiring testleri (gsb-core, 7 yeni test)

`gsb_core::room`'daki altı terminal ret kararının her biri kendi kovasına
sayıyor; `crates/gsb-core/tests/rpc.rs` artık her yol için bir wiring
testi taşıyor. Test altyapısı: harness odanın metrik kanalını TUTUYOR
(cap 64; diğer testler drain etmiyor — dolan kanal yalnız odanın kendi
`metrics_dropped`'ünü sayar, davranış değişmez) ve `latest_room_sample`
bounded kanalı boşaltıp en yeni `RoomSample`'ı döndürüyor; `bucket_cfg`
örnekleme temposunu adım başına kuruyor (varsayılan 1 Hz/30 Hz'de kısa
test hiç örnek görmeyecekti). Senkronizasyon: reply okuması
(`private_replies`), o reply'yi üreten adımın (ve adımın SON işi olan
metrik `try_send`'inin) tamamlandığının garantisi — tek-thread test
runtime'ında oda görevi `tick_rx.recv()`'de yield etmeden adımın tamamını
çalıştırır. Her test taze odada tam olarak BİR ret yolunu tetikler ve
doğru kovanın arttığını **VE diğer beşinin artmadığını** assert eder
(ikinci yarısı — her şeyi artıran, ya da yanlış kovayı artıran sayaç
burada düşer):

- `reject_bucket_malformed` — çöp envelope (decode başarısızlığı) +
  `id = 0` (kovanın iki kaynağı, ikisi de buraya sayılır);
- `reject_bucket_dup` — in-flight external istek + aynı tick'te AYNI id'li
  ikinci istek (dup kararı decision'ün ÜSTÜNDE: room-local op bile
  burada reddedilir, in-flight olan işlemeye devam eder);
- `reject_bucket_no_handler` — `handle_request`'in `None` döndürdüğü op
  (`OP_UNKNOWN`; kurulumu zor değil: `RpcLogic` dört bilinen op dışında
  her şeye `None` döndürüyor);
- `reject_bucket_logic` — mantığın kendi `RequestDecision::Reject`'i
  (`OP_REJECT`);
- `reject_bucket_conn_cap` — per-connection cap = 1, aynı tick'te aynı
  conn'dan 2 external istek (birinci kaydolur: 0 < 1; ikinci 1 ≥ 1'de
  cap'e çarpar; room cap 2000'de uzak);
- `reject_bucket_room_cap` — room cap = 1: conn 1 odanın tek slotunu
  doldurur, conn 2'nin isteği (KENDİ count'u 0 iken) ROOM kovasına
  düşmeli — `conn_cap == 0` yarısı yanlış kablolamayı yakalar;
- `reject_bucket_both_caps_prefers_conn_cap` — iki cap de aşıldığında
  per-connection kovası kazanır (kod `over_conn_cap`'e önce bakar —
  istemcinin kendi kotası actionable olan sınırdır).

### B — Sayaç envanteri (kapsama kriteri: doğru yolda artışı doğrulayan test)

Kapsama yüzeyi: `RoomSample` (oda actor'ü), `RegistrySample` (registry),
`ConnSample` (connection actor'ü), `UdpClientStats` (rUDP istemci;
RESULT satırında). **evet** = test yolu tetikliyor ve artışı/gauge
değerini assert ediyor; **smoke** = test sayaç okuyor ama yalnız
sıfır/salimlik (doğru yol doğrulanmıyor); **hayır** = sayaça dokunan
test yok.

**RoomSample:**

| Sayaç | Kapsam |
|---|---|
| `steps` | evet — `room_counters_flow_to_collector` (≥4 + monoton), loadgen smoke (≥60) |
| `joins` | evet — flow test (join → ==1) |
| `members`, `groups`, `max_group` (gauge) | evet — flow test (join → 1/1/1) |
| `snapshots` | evet — flow test (emit eden mantıkla >0) |
| `dropped_frames` | evet — flow test (tüketilmeyen out kanalı → >0) |
| `step_max_us`, `step_hist` | evet (flow) — `step_max_us > 0`; `sum(step_hist) == steps`; bin kenarları ayrı unit testli (`hist_index_binning`) |
| `step_min_us`, `step_sum_us` | hayır |
| `step_fine_hist` | hayır (saf `fine_hist_index` fonksiyonu testli; oda tarafı birikim değil) |
| `late_min_us`, `late_max_us`, `late_sum_us` | hayır |
| `lagged_events`, `lagged_ticks` | hayır (broadcast buffer'ı doldurup okuyan test yok) |
| `dropped_actions` | hayır — yapısal N/A: oda bounded *pull*, aksiyon asla atmaz; sayaç 0'da kalır |
| `keepalive_resends` | hayır (davranış testli: `unchanged_group_is_silent_until_keepalive`; sayaç okunmuyor) |
| `snap_bytes`, `snap_bytes_max` | hayır |
| `snap_overflows` | hayır (MTU uyarısı yalnız ölçümde gözleniyor) |
| `snap_records` | hayır |
| `shipped_bytes`, `shipped_frames`, `private_frames` | hayır (rapor-düzeyi smoke: `server_out_bps > 0`) |
| `leaves` | hayır (rpc testleri leave davranışını test ediyor; sayaç okunmuyor) |
| `requests_local`, `requests_external` | hayır (reply davranışı rpc.rs'te testli; sayaç okunmuyor) |
| `requests_rejected_{malformed,dup,no_handler,logic,conn_cap,room_cap}` | **evet — bu tur (A)** |
| `requests_timed_out`, `requests_late` | hayır (timeout/stale-report davranışı testli; sayaç okunmuyor) |
| `pending_requests` (gauge) | hayır |
| `metrics_dropped` | smoke (smoke rapor toplamını ==0 assert ediyor) |

**RegistrySample:** `rooms`/`conns` (gauge) — smoke (loadgen
`peak_conns >= 3`); `rooms_created`, `rooms_destroyed`, `joins`,
`leaves`, `opens`, `closes` — hayır (`tests/registry.rs` join/leave/
create davranışını frame üzerinden test ediyor; örnek sayaçları
okunmuyor); `metrics_dropped` — hayır.

**ConnSample:** `actions_dropped` — **evet** (`e2e::flooder_drops_
attributed`: flood → `>0` + flooder'a atıf); `bytes_in`/`bytes_out` —
smoke (`server_in/out_bps > 0`); `frames_in`, `frames_out`,
`violations` (bütçe davranışı `violation.rs`'te testli; sayaç
okunmuyor), `last`, `metrics_dropped` — hayır.

**UdpClientStats** (rUDP istemci): `retrans_out`, `dup_in`,
`oob_dropped`, `gave_up` — hayır (transport davranışları `gsb-net`'te
testli: reorder/dedup/retransmit/oversized-drop; sayaçlar okunmuyor).

**Komşu alanlar (sunucu metrik yüzeyi DIŞINDA — envanter görünür olsun
diye listelenir, adlandırılmış maddenin kapsamı değildir):** loadgen'in
kendi gözlem sayaçları (`ClientReport`: `moves`, `errors`,
`join_rejected`, `cap_rejected`, `budget_rejected`, `acks`,
`ack_processed_max`, `ack_lag_max_ms`, `fulls`, `private_fulls`,
`deltas`, `gap_drops`, `view_size`, `still_frac`) — araç bunlarla
ölçer; e2e tetik semantiğini (hata kodları) kısmen doğruluyor ama
sayaç düzeyinde test yok. Demux/writer teşhis sayaçları (yalnız log,
örnek yok: `established`, `bad_cookie`, `oversized_in`, `gave_up`,
`retransmits`, per-session `oob_dropped`/`dup_in`/`inbox_full`, …)
rapor yolu taşımıyor.

### C — `.measure/` hijyeni

Bir önceki turun loadgen ham çıktıları (A/B koşuları, binary'ler)
`.measure/` altında untracked'di; rapor `git status --porcelain`
çıktısına bakmadan "ağaç temiz" demişti (iki tur önce `.scratch/`'le
birebir aynı hata). Seçim: **`.gitignore`'a ekleme** — `.scratch/`
emsali (ölçüm ham çıktıları yerel çalışma verisi); silmek ayrıca
yanlış, çünkü bu dosyalar önceki turun karşılaştırma tabanları.
Doğrulama: `git status --porcelain` bu turun sonunda gerçekten boş
(değişiklikler commit'lenmeden önce yalnız bu turun iki dosyası).


## Kapatılanlar (ölçüm çözünürlüğü + taban turu)

**Tur kapsamı.** Spec'ten iki madde: (A) **ölçüm çözünürlüğü** — adım
zamanı metrikteki log2 histogram'ın binleri 2× ayrık (30 Hz bütçede
391/782/1563 etiketleri); ortaçağın %26'lık eğrisi "tek kovanın içine
sığınıyor", dışarıdan p50=391 her 4 frac'ta aynı görülüyor — bütçenin
çok altındaki bölgede **%10-20'lik değişimleri ayırt eden** bir ölçüm
gerekli (metod serbest); BÜTÇE-ASHIM sinyali dokunulmaz kalmalı: "(1,1)
kenarı = tick bütçesi ve üstü = aşım semantiği duruyor; yeni
çözünürlük onun yerine değil, **yanına** gelmeli"; hot path'te float
yok. (B) **durgunluktan bağımsız taban** — önceki turun 270-310
µs/tick tabanı bağlantı sayısıyla (hücre sayısı ile değil) ölçekleniyor;
ÖNCE ATFET, SONRA OPTİMİZE ET: "tahminle değil ölçümle"; en büyük
dilime hedef, ≥2 gerçek alternatif değerlendirmeli. Wire protokolü
**değişmedi** (iç optimizasyon + ölçüm); Mutex/RwLock/parking_lot/
select! yok; `unsafe_code = "forbid"`; 120 mevcut testin hiçbiri
silinmedi/`#[ignore]`'lenmedi.

### A — İnce adım-histogramı (sabit 8 µs binler, log2'ye YANINA)

**Ne.** `metrics.rs`'e log2 histogram'ın yanına ikinci, **sabit 8 µs
binli** histogram eklendi: `FINE_HIST_US_PER_BIN=8`,
`FINE_HIST_BINS=512`, tavan 4096 µs (`[0,4096)`). Log2 histogram **birebir
aynı** kalıyor: `(1,1)` kenarı = tick bütçesi, `HIST_OVERFLOW_BIN`,
`over_budget_frac` ve RESULT'taki `step_p50_us~`/`step_p99_us~`
etiketlerinin tümü aynen duruyor; ≥4096 µs olan adımlar yalnız log2
histogram'da görünüyor (aşım semantiği bozulmadı — spec: "yanına
gelmeli"). Hot path maliyeti: tick başına **bir saturating u32
artış** (tam sayı; float yok — spec kısıtı). Percentil raporlama
anında, bütünüyle tamsayı aritmetik: `fine_hist_percentile_us(hist,
total, p)` = "en az p% adımın ≤ L+7 olduğu en küçük bin alt kenarı L"
(`total` = oda ADIM SAYISI — tavan üstü adımlar da dahil; percentil
sırası tavana dayanırsa `None` → kabaca log2 tahmini hâlâ
mevcut). `RoomSample`'a `[u32;512]` (2 KB — örnek her 30 adımda, hot
path DIŞINDA kopyalanıyor), `RoomReport`'a `[u64;512]` (shard katlama
toplamı) eklendi. Loadgen: `fold_rooms` eleman-eleman toplar;
`server room (final)` + RESULT satırlarına `step_p50_fine_us` /
`step_p90_fine_us` (sıra tavan üstündeyse işareti = 4096;
belirsizlik yok: ince bin alt kenarları asla 4096 olmaz, en çoğu
4088). Served/orchestrated moda: stream formatı **GSM2→GSM3**
(oda başına +512×u32; format dokümanı güncellendi).

**Neden SABİT bin (bütçe fraksiyonu değil).** Çözünürlük hedefi,
adım zamanlarının GERÇEKTE yaşadığı bölgede (yüz µs mertebesi) mutlak
µs farklarını ayırt etmek — tick hızından bağımsız. Bütçe-fraksiyonlu
ince bin, 2× mesafeli bin problemini yalnız yeniden ölçekler
(fraksiyonlar arası oran yine 2×'e yaklaşır); mutlak 8 µs bin, %10-20
farkı her bütçede 1-3 bin adımla ayırır. Test kilitliyor (aşağı).

**Kilitli testler (spec gereği: sentetik dağılım → percentil
doğruluğu).** (i) `fine_hist_percentiles_known_distributions`: düzgün
[0,4096) (1000 adım, 2/bin) → p50 = 249×8 = **1992**, p99 = 494×8 =
3952; bimodal 500@390 µs + 500@780 µs (ölçülen kümeler) → p50 = 384,
p99 = 776; yamutlu (77 adım tek bin) → p1 = p100 = bin kenarı. (ii)
`fine_hist_cap_and_overflow`: tavan sınırı `index(4095)=Some(511)`,
`index(4096)=None`, `index(u64::MAX)=None`; 100 adım tavan altı + 50
tavan üstü (total 150) → p50 tavan altı binde, **p99 = None** (sıra
tavana dayanıyor); boş histogram / p∉[1,100] → None. (iii)
`fine_hist_resolves_ten_percent_difference`: 1000 adım @390 vs @430 µs
(+%10.3) → ince p50'ler **384 vs 424, ayrı**; aynı iki değer 30 Hz
bütçede log2'de **aynı binde** (sanity assertion — kusur sınıfının
tanımı). Mevcut `hist_index_binning` testi **değişmeden** duruyor
(aşım semantiğinin bekçisi).

**Elenen alternatifler.** (1) *Ham örneklerden gerçek percentil*
(reservoir / sıralama istatistiği) — hot path'e adım başına µs
değeri depolamak (ring buffer + actor sınırı) ve raporda O(n log n)
gerektirir; histogram adım başına bir artıştır ve sub-µs hedef için
8 µs bin yeterince kesindir (hata < 8 µs, percentil bin alt kenarı
semantiğinde belgelenir). (2) *Daha ince log2 binleri* (örn. 4. kök)
— 2× boşluk kalır, sadece daha çok bür; çözünürlük sorunu yapısal.
(3) *Log2 histogramı GENİŞLETMEK (yerine koymak)* — spec'e açık
ters: aşım semantiği "yanında" kalmalı.

### B — Taban: önce atfet, sonra optimize et

**Ölçüm kurulumu (before/after nasıl kuruldu).** Geçici 14-kapsamlı
faz probe'u (`GSB_PHASE` env-var kapılı; commit'te **yok**, tur sonunda
kaldırıldı — `grep -r phase_probe crates/` → 0):
`control / read / convert / sys / stamp / dirty / group_of / c4b / c4c
/ snap / ka / recs / fanout / priv`, 90-tick (3 s) pencerelerde
µs/tick. İç içe kapsamlar (rapor SATIRLARI ham toplamdır, gerçek
faz = fark): `sys ⊇ stamp ⊇ dirty`, `c4c ⊇ snap + ka`,
`fanout ⊇ priv`. İki kod durumu sıralı ölçüldü: **BEFORE** = aa705ea +
A (madde B YOK) ve **AFTER** = aa705ea + A + B (teslim edilen), aynı
makine (Ryzen 9 7950X, 16C/32T), **pinned** `taskset -c 8-15`, release,
in-proc (500 istemci + 1 oda, tek worker), `--duration 30 --profile
still --cell-size 20 --visibility spatial`; her koşunun loadavg'ı
meta dosyada (BEFORE 3.90-4.23, AFTER 3.43-4.97). Bir AFTER koşusu
**kontamine** bulundu ve **atıldı** (load 7.8, `dropped=2`,
`leaves=169`, makinede 99% CPU'lu yabancı python) — "kanıt değil".
Probe'nın kendisinin step zamanına ek maliyeti her iki durumda aynı
(≈20-30 µs; mod karışımı nedeniyle koşu koşu değişir — tablo
karşılaştırmada iki yanına eşit dağılır).

**Atfetme (BEFORE, µs/tick, istikrarlı pencereler w2-10, "real" = fark):**

| faz | 0.90 (real) | 0.00 (real) | not |
|---|---:|---:|---|
| control | 0.7 | 0.8 | |
| read | 36.8 | 48.6 | 500× try_recv (bağlantı kutusu) |
| convert | 6.2 | 35.0 | ingest (hareketle ölçekli) |
| sys (total) | 52.1 | 122.0 | |
| └ sys_real (−stamp) | 13.4 | 21.1 | |
| └ stamp_real (−dirty) | 9.1 | 8.9 | |
| dirty | 29.6 | 92.0 | bevy change-detection + mover churn |
| group_of (4a) | 38.8 | 31.0 | 500× (2 tablo get + ara sıra ECS) |
| c4b (members tablosu) | 22.9 | 13.4 | tick başına taze HashMap + Vec'ler |
| c4c (total) | 32.5 | 24.0 | |
| └ c4c_real (−snap−ka) | 3.8 | 1.8 | döngü yapıştırması |
| snap | 27.0 | 21.4 | grup başına (16/4 grup) |
| ka | 1.7 | 0.8 | |
| **fanout_real (4d−priv)** | **153.2** | **163.7** | **EN BÜYÜK dilim (her frac'ta)** |
| **priv** | **85.9** | **96.7** | **ikinci dilim (her frac'ta)** |
| **Σ (real)** | **429.1** | **535.2** | step_mean (probe'lu) 459.1 / 549.4 |

Bulgu: (i) spec'in ipucu ("ack her tick private kare üretmeli mi")
**ölçümle cevaplandı: hayır** — `emit_ack` zaten on-change (önceki
tur); 0.90'da priv maliyeti kare ÜRETİMİ değil, `private()`'ın
arama zinciri (conn_entity + last_cell + conn_view + input). (ii)
Taban hücre değil **bağlantı** ile ölçekleniyor: en büyük iki dilim
(4d fan-out yapıştırması + priv) ikisi de bağlantı-bazlı. (iii)
Önceki turun "270-310 µs" (priv 92-106 + core 177-208) tahmini bu
dökümle doğrulandı: fanout_real+priv ≈ 239-260 µs.

**Karar (en büyük dilime hedef; ≥2 gerçek alternatif değerlendirildi).**
1. **Bağlantı-bazlı batch Vec yeniden kullanımı (4d).** Eski: tick
   başına bağlantı başına `Vec::with_capacity(2)` = **500 heap
   tahsisi/tick** (0.90'da ~310 bağlantının batch'i boşken bile). Yeni:
   `RoomConn.batch` kalıcı alan; tick başına `clear()` (kapasite
   korunur), `std::mem::take` ile kanala teslim (sıfır tahsis),
   `try_send` başarısızsa `TrySendError::into_inner` ile **buffer geri
   konur** (bir sonraki tick de yeniden kullanır). Davranış kilidi:
   `full_outbound_channel_drops_then_recovers` testi (70 emission / 64
   slot → kanal TAM olarak 64 sağlam, sıralı batch tutar; boşluk
   açılınca SONRAKİ emission aynen gelir — wedged bağlantı/buffer yok).
2. **`private()`'e group'un verilmesi (trait değişikliği).** Oda, 4a'da
   zaten hesapladığı `rc.group`'u `RoomLogic::private(&mut world,
   conn, &group, out)` olarak geçirir; `AoiRoom::private` artık
   `conn_cell` ile grubu yeniden TURETMEZ (2 tablo get × 500
   bağlantı/tick gider; `conn_cell` methodu öldü — silindi). Trait
   dokümanı gerekçeyi taşır ("re-türetemezsin: her re-türeteme =
   bağlantı başına ek aramalar, ölçüldü"). Shard odası (aynı
   `RoomConn`/`try_send` deseni) birebir aynı düzeltmeyi aldı.

**Elenen alternatifler (gerçek, ölçülen gerekçeli).** (1) *4b members
tablosunun yerinde yeniden kullanımı / artımlı yama* — gerçek alternatif
(önceki turun in-place kalıbı), ama dilim küçük (13-23 µs) ve rebuild
kalıbı savaşmış; group-staleness hata sınıfı (gone-removal, max_group
yeniden hesaplama) ~20 µs için değmez. (2) *READ kapı zili / boş çekim
atla (37-49 µs)* — en karmaşık değişiklik (bağlantı-bazlı
bildirim kanalı), drop semantiğine dokunur; en büyük iki dilimin
altında. (3) *group_of'un tick-ler arası önbelleği (31-39 µs)* —
trait'e "grup değişti" bildirim kanalı gerekir; ~35 µs için protokol
karmaşıklığı. (4) *fan-out topolojisi: grup→üye iterasyonu (4d'deki
500× `groups.get`'i kaldirır)* — gerçek alternatif; REDDEDİLDİ: 500
`groups.get` yerine 500 `conns.get_mut` koyar (aynı mertebe) ve (1) ile
etkileşir; (1)+(2) sonrası fanout_real'in kalanı `try_send` (kanal
teslimi) — wire değişikliği olmadan indirilemez. (5) *dirty yeniden
yazımı (94 µs @0.00)* — bevy change-detection + mover HashMap churn;
yerine göre bağımlı (taban değil) dilim için SoA-yapısal yeniden
yazım. (6) *ack'in her tick private kare üretmesi* — ölçümle elendi
(yukarı (i); on-change zaten).

**Öncesi/sonrası, bileşen bileşen (probe, istikrarlı pencereler, real, µs/tick):**

| faz | BEFORE 0.90 | AFTER 0.90 | Δ | BEFORE 0.00 | AFTER 0.00 | Δ |
|---|---:|---:|---:|---:|---:|---:|
| control | 0.7 | 0.4 | −0.3 | 0.8 | 0.5 | −0.3 |
| read | 36.8 | 23.6 | −13.2 | 48.6 | 42.7 | −5.9 |
| convert | 6.2 | 3.9 | −2.3 | 35.0 | 27.9 | −7.1 |
| sys_real | 13.4 | 9.0 | −4.4 | 21.1 | 17.4 | −3.7 |
| stamp_real | 9.1 | 6.1 | −3.0 | 8.9 | 6.6 | −2.3 |
| dirty | 29.6 | 21.1 | −8.5 | 92.0 | 81.5 | −10.5 |
| group_of | 38.8 | 31.7 | −7.1 | 31.0 | 29.9 | −1.1 |
| c4b | 22.9 | 16.9 | −6.0 | 13.4 | 11.2 | −2.2 |
| c4c_real | 3.8 | 2.5 | −1.3 | 1.8 | 1.2 | −0.6 |
| snap | 27.0 | 22.9 | −4.1 | 21.4 | 18.4 | −3.0 |
| ka | 1.7 | 1.4 | −0.3 | 0.8 | 0.7 | −0.1 |
| **fanout_real** | **153.2** | **110.3** | **−42.9** | **163.7** | **135.7** | **−28.0** |
| **priv** | **85.9** | **58.3** | **−27.6** | **96.7** | **74.8** | **−21.9** |
| **Σ real** | **429.1** | **308.1** | **−121.0** | **535.2** | **448.5** | **−86.7** |

Okuma uyarısı (bimodalite): AFTER pencereleri hızlı modda, BEFORE
pencereleri yavaş modda oturdu (önceki turun belgelenen koşu-kuşu mod
karışımı; mekanizma izole edilmedi). **Dokunulmamış** dilimler de (read,
dirty, group_of, c4b, snap...) mod kaymasıyla düştü; optimize edilen
dilimler (fanout_real −42.9/−28.0, priv −27.6/−21.9) toplam **−70.5/−49.9
µs** = Σ düşüşünün ~%58'i ölçümlenen optimizasyondan, kalanı mod
kayması.

**Taban (probe'sİZ, temiz koşular, pinned, N=500) — acceptance:**

| still_frac | BASE2 step_mean (aa705ea, önceki tur) | FINAL step_mean (bu tur) | Δ | FINAL fine p50 / p90 | FINAL log2 hist |
|---|---:|---:|---:|---|---|
| 0.00 | 492.6 | 490.4 | −2.2 | 464 / 648 | [16,594,288,2] |
| 0.50 | 499.2 | 387.4 | **−111.8** | 376 / 512 | [79,736,85] |
| 0.90 | 433.5 | **290.7** | **−142.8** | **264 / 440** | [428,443,28,1] |
| 0.99 | 362.3 | 241.8 | **−120.5** | 224 / 384 | [557,323,19,1] |

Aynı N=500'de taban **ölçülmüş olarak düştü** (0.90: 433.5 → 290.7
µs; −%33). A'nın ince histogramı bunu ilk kez GÖRÜYOR: 0.90'da adımların
%47'si 260 µs altı (BASE2 koşularında bin0 kitlesi ~%10-30 idi), fine
p50 = 264 µs — log2'de iki koşu da "p50~391" görünürdü (çözünürlük
kusurunun kendisi). `over_budget_pct=0.0` tüm 4 frac'ta (aşım sinyali
dokunulmadı), `dropped=0`. 0.00'deki küçük Δ (−2.2) beklendiği
gibi: tam-hareket durumunda adım zamanını dirty (92→81) + convert
(35→28) + mod karışımı belirliyor — optimize edilen dilimler (fanout/
priv) orada da düştü (probe tablosu) ama step_mean'i başka dilimler
taşıyor.

**Ham RESULT satırları (kesintisiz, FINAL koşusu, pinned, load 3.4-4.9):**

```
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=444867 snap_per_client_p50=891.0 tick_hz_med=30.00 client_in_bps=23566588 client_out_bps=36677 out_bps_per_conn=46913 moves=88895 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_p50_fine_us=464 step_p90_fine_us=648 step_max_us=1559 step_over_budget_pct=0.0 dropped=0 late_max_us=1672 peak_payload_b=4288 snap_overflows=2122 records_per_tick=192.7 overlap_x=0.39 server_in_bps=24625 server_out_bps=23456415 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=88892 ack_processed_max=178 ack_lag_max_ms=108 fulls=32854 private_fulls=18178 deltas=429827 gap_drops=364 view_size=250000 still_frac=0
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=447904 snap_per_client_p50=897.0 tick_hz_med=30.00 client_in_bps=10185714 client_out_bps=18802 out_bps_per_conn=20170 moves=45000 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_p50_fine_us=376 step_p90_fine_us=512 step_max_us=765 step_over_budget_pct=0.0 dropped=0 late_max_us=1733 peak_payload_b=3255 snap_overflows=337 records_per_tick=110.9 overlap_x=0.22 server_in_bps=12602 server_out_bps=10085171 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=45000 ack_processed_max=179 ack_lag_max_ms=106 fulls=24924 private_fulls=10248 deltas=432865 gap_drops=363 view_size=135088 still_frac=0.5
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=446987 snap_per_client_p50=896.0 tick_hz_med=30.00 client_in_bps=3642304 client_out_bps=4289 out_bps_per_conn=7101 moves=9401 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_p50_fine_us=264 step_p90_fine_us=440 step_max_us=1479 step_over_budget_pct=0.0 dropped=0 late_max_us=1550 peak_payload_b=2768 snap_overflows=356 records_per_tick=46.1 overlap_x=0.09 server_in_bps=2836 server_out_bps=3550289 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=9400 ack_processed_max=179 ack_lag_max_ms=102 fulls=18149 private_fulls=3493 deltas=431917 gap_drops=414 view_size=84346 still_frac=0.9
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=321220 snap_per_client_p50=664.0 tick_hz_med=30.00 client_in_bps=2644365 client_out_bps=1015 out_bps_per_conn=5168 moves=1370 errors=0 steps=900 server_hz=30.00 step_p50_us=130 step_p50_fine_us=224 step_p90_fine_us=384 step_max_us=1346 step_over_budget_pct=0.0 dropped=0 late_max_us=749 peak_payload_b=2682 snap_overflows=366 records_per_tick=45.0 overlap_x=0.09 server_in_bps=633 server_out_bps=2583946 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=1370 ack_processed_max=175 ack_lag_max_ms=68 fulls=16700 private_fulls=2019 deltas=306124 gap_drops=415 view_size=77665 still_frac=0.99
```

(tam satırlar `.scratch/FINAL_*.txt`; BASE2 satırları
`.scratch/BASE2_*.txt` — önceki tur.)

### Tur özeti (yeni testler, hiçbir eski test silinmedi/ihmal edilmedi)

- **4 yeni test**: A için 3 sentetik-distribusyon testi
  (`fine_hist_percentiles_known_distributions`,
  `fine_hist_cap_and_overflow`,
  `fine_hist_resolves_ten_percent_difference` — spec'in
  "bilinen dağılımı doğru rapor ettiğini kilitle" maddesi) + B için 1
  davranış kilidi (`full_outbound_channel_drops_then_recovers`:
  mem::take/into_inner geri-kazanım yolu). Mevcut 6 delta testi
  (`tests/delta_aoi.rs`) ve 4 ack testi (`tests/input_ack.rs`)
  **değişmeden** yeşil (private()'nin group argümanı davranış
  koruduğunu kanıtlıyor: geç giriş full, one-shot, NPC-hücre doğumu,
  ack monotonluğu).
- **120 → 124** (124/124 yeşil + 1 var olan `#[ignore]`'li gsb-lint
  doctest; 0 silinen, 0 ihmal edilen). `cargo test --workspace` +
  `cargo clippy --workspace --all-targets` temiz (0 uyarı), her
  kaynak dokunuşundan sonra derleme satırlarıyla.
- **Yapılmayanlar (açık beyan):** (1) 2× bimodalitenin
  mikromimari mekanizması hâlâ izole edilmedi (önceki turda olduğu
  gibi gipotez olarak kalır; bu turda mod karışımı B/A tablolarını
  kirletmedi — her iki koşu da istikrarlı pencerede alındı ve fark
  tabloda okuma uyarısıyla açıklandı). (2) READ fazı (500× try_recv)
  optimize edilmedi — dilim en büyük ikisinin altında, değişiklik
  kanal semantiğine dokunur (gelecek tur adayı). (3) group_of'un
  tick-ler arası önbelleği yapılmadı (trait bildirimi gerektirir).
  (4) 4b members tablosu artımlı yapılmadı. (5) Served/orchestrated
  moda GSM3 stream'i uçtan uca koşulanmadı — encode/decode simetrik
  derlendi, in-proc (stdout) modu smoke testleriyle doğrulandı;
  binary stream testi bu turda yok. (6) Probe altyapısı tur sonunda
  **kaldırıldı** (commit'te iz yok).

---



## Kapatılanlar (AOI per-cell memo + dirty cell turu)

**Tur kapsamı.** Spec'ten iki madde: (A) per-cell sınıflandırma
memo'laması — sınıflandırma tick başına hücre başına **bir kez** hesaplanır
(NEGATİF `Silent` sonuçlar dahil), inner bucket map'leri yerinde temizlenir;
(B) dirty hücre seti — tick başına iş **hareket eden** entity ile orantılı
olacak; dirty işaretini **yapısal** olarak garanti eden tasarım (en az iki
gerçek alternatif değerlendirmeli; proje "yorumda disiplin"den üç kez
yaralanmıştır: `bump()`/F1, `WireId` ön koşulu, `last_sent` haritası).
Kilitli kabuller aynen korundu: 6 delta testi (`tests/delta_aoi.rs`)
değişmeden yeşil (delta akışı ≡ full akışı istemci görünümü; 3 gözlemci
pozisyonunda hayalet/kopya yok; hücre çıkışında entity istemcide silinir;
geç giriş `private()` full'ı; kayıp keepalive sınırında iyileşir). Wire
protokolü **değişmedi** — saf iç optimizasyon (proto dosyasında bu turda
değişiklik yok).

### A — Per-cell sınıflandırma memo'laması (+ inner temizlik)

**Ne.** `AoiRoom::classify` artık tick başına hücre başına bir kez hesaplayıp
sonucu `frag_cache: HashMap<Cell, CellFrag>`'e koyuyor — **negatif
sonuçlar dahil** (içerik aynı olan `(true, true)` hücresi = `Silent` da
önbellekleniyor). Gerekçe (kod dokümanında da): bir tick içinde
sınıflandırma, (önceki, güncel) bucket çiftinin **saf fonksiyonu**dur ve bu
çift `update`'e kadar donuktur — dolayısıyla ilk çağıranın sonucu her
sonraki çağrıcı için (diğer tüm gruplar + 3×3 bloğunun **4 kez**
yürütülmesinin her geçişi) kesindir; memo yaklaşıksa değil, **doğrudur**.
`Silent` memo'sunun hedefi ölçülen kusurdu: eski `delta_piece`, sessiz
hücrede `None` dönerken sonucu `delta_pieces`'e **yazmıyordu** → her
sonraki `classify` çağrısı hücrenin TAM içeriğini yeniden diff'liyordu
(grup×geçiş sayısı kadar, tick başına). Ayrıca spec (a)-c: bucket
rotasyonunda taze map'in **iç** map'leri yerinde `clear()` ile temizleniyor
(dış anahtarlar + iç tahsisler rotasyonda yaşıyor — tick başına tahsis
git-alaşı yok). `born_groups` türetimi değişmedi (üye hücre kümesi farkı).

**Kapanan kusur sınıfı.** "Tick içinde değişmeyen cevabı birden fazla kez
hesapla" — burada maliyeti taşıyan varyant NEGATİF cevaptı: sessiz hücre
(basınçlı durumda hücrelerin büyük çoğunluğu) her ziyarette yeniden diff
ediliyordu. Ölçülen etki (faz dökümü, aşağı): snap fazı 254 µs/tick →
53 µs/tick (0.90, karşılaştırılabilir koşu).

**Elenen alternatifler.** (1) *Eager: tüm hücrelerin parçasını tick başında
üret* — 3×3 görüş yerel: grup yalnızca kendi bloğundaki hücreleri ziyaret
eder; eager, hiç ziyaret edilmeyecek dolu hücrelerin parçasını da üretir
(O(dolu hücre) × diff, grup sayısından bağımsız) — lazy + memo
O(ziyaret edilen × 1 diff). (2) *Entity-bazlı versiyon damgası (bump() geri
getir)* — B maddesinde elenen disiplin sınıfının ta kendisi (F1 tarihi);
ayrıca sınıflandırma zaten bucket çiftinin saf fonksiyonu — ek versiyon
bilgisi taşımaz. (3) *Statüko (yeniden diff)* — ölçülen 254 µs/tick.

**Spec sapması (b) — "born_groups bucket diff'inden":** spec'in harfiyle
"bucket diff'i" (hücre önceki ve güncel bucket'larda VAR) doğum kuralı
olarak yetmez: yalnızca **üye olmayan** içerik taşıyan (NPC) bir hücre
her iki tick'te de doluysa ve bu tick'te ilk üyesini alıyorsa, harfi kural
doğumu **kaçırır** (hücre diff'de yeni değil). Uygulanan kural üye
sayımıdır: `before = now − in + out` (sıra-bağımsız; aynı tick'te
çıkış+giriş sahte doğum üretmez) — üye-only durumlarda harfi okumayla
aynı sonucu verir, genel durumda doğrudur. Kaçırılan doğumun gözlemsel
sonucu zararsız olurdu (üye bir seferlik private full ile baseline'lanır),
ama uygulanan seçim grubun full'unu yayınlar (superset; aynı baytlar;
private atlanır, `group_full_emitted` üzerinden). Test:
`aoi_member_join_npc_cell_born_full` (modül dokümanına sapma notu yazıldı).

**Spec hatası (A beklentisi) — ÖNEMLİ:** spec, A sonrası süpürmeyi "değerler
düşer ama **hâlâ düz**" olarak öngörüyordu. Öyle DEĞİL, ve mekanizması
belirgin: A'nın el ettiği maliyet (sessiz hücrelerin yeniden diff'i)
**tam olarak hareketsizlikle ölçeklenendir** — hareketsiz hücreler ancak
entity'ler oturduğunda var olur (wire kuantizasyonu: hareket eden entity
bile i32 wire konumunu her ~3 tick'te değiştirir → aktif hücre
"hareketsiz" sınıflandırması almaz). Ölçülen: BASE 0.90/0.99 adım ortalaması
720/548 µs → A 515/449 µs; p50 log2-kanıtı 782-bin'den 391-bin'e iniyor
(0.90/0.99; 0.00/0.50'da sessiz hücre yok → memo işe yaramaz, aynı kalır).
Yani "düz kalması beklenen" süpürme, A'da zaten eğimli; B maddesinin
acceptance ölçütü bu gerçeğe karşı yorumlandı (aşağı).

### B — Dirty hücre seti (yapısal işaret)

**Ne.** Tick başına iş, dünya içeriğinin yeniden türetimi olmaktan çıktı:
- İşaret **yapısal**: `Position` bileşenine yazan HER yazan (sistem,
  ingest, test yardımcıları, gelecekteki yeni sistemler) bevy'nin
  bileşen-bazlı change tick'iyle otomatik işaretleniyor; oda
  `query_filtered::<(Entity, &WireId, &Position), Changed<Position>>`
  ile tick penceresini okuyor ve yalnızca işaretli entity'ler için
  bucket/defter işi yapıyor. Standalone bevy'de pencereyi **oda elle
  ilerletir**: her `update()` sonunda `world.clear_trackers()` (=
  `increment_change_tick()`) — pencere "önceki güncelleme → bu
  güncelleme arası yazımlar". Despawn bir bileşen yazımı olmadığı için
  çıkışlar odanın üyelik defterinden geliyor: `on_leave` çıkışı
  `pending_removals`'a park eder (entity hem `WireId` hem `last_cell`
  taşıyorsa — yani gerçekten bucket'lanmışsa), `update` park edilen
  çıkışları değiştirim listelerine uygular.
- Defter: `prev_occupied: HashSet<Cell>` (dirty döngüsü boyunca DONDURULUŞ,
  tick sonunda dokunulan hücreler için rulo), `last_cell: HashMap<Entity,
  Cell>` (ayn zamanda `group_of`/`conn_cell`'in O(1) kaynağı),
  `member_counts: HashMap<Cell, u32>`, `touched: HashMap<Cell, TouchInfo>`
  (üye giriş/çıkış sayıları — sıra-bağımsız doğum aritmetiği),
  `cell_changes: HashMap<Cell, CellChanges>` (updates/exits listeleri +
  appeared/exited bayrakları), `pending_removals: Vec<(Entity, u64, Cell)>`.
- `classify` artık değişim listesini okuyor (A'nın memo'su aynen):
  listede yok → `Silent`, `appeared` → `Appeared`, `exited` → `Exited`,
  var → `Delta`. `delta_piece` artık **diff yapmıyor** — hücrenin değişim
  listesini doğrudan kodluyor. Quantization no-op (f32 konum değişti ama
  i32 wire konumu aynı) → kayıt yok, hücre `Silent` (kayıt
  gözlemsel olarak değişmedi — full/delta akışları hâlâ aynı istemci
  görünümünde).
- Doğum kuralı: üye sayımı (yukarıdaki spec-(b) sapması). Hücre
  `before = now − in + out` hesabıyla 0'dan üyeye geçiyorsa doğar; bu,
  NPC-only hücreye üye katılması durumunu da kapsar.

**§7 ile ilişki (spec sorusu):** DESIGN §7, bevy'nin **event/observer**
mekanizmasını hot path'ten çıkarmıştı (bevy 0.19'da yeniden tasarım —
churn riski; ayrıca hot path'te event tamponu = yazım başına tahsis +
drain döngüsü). B maddesi bu elenmeyle **çelişmiyor**: kullanılan bevy
arayüzü event/observer değil, **query filtresi** (`Changed<T>` change
tick'i) — ve standalone modda scheduler olmadığı için baseline'ı oda elle
ilerletiyor (`clear_trackers`). §7, "strateji bazında değişim sinyali"
şeklinde güncellendi (demo stratejisi: wire içeriği; spatial: change tick'i).

**Kapanan kusur sınıfı.** (1) "Tick başına iş, dünya toplamı ile orantılı"
— üç O(N) world taraması (rebuild sorgusu, doğum taraması, `group_of`
world fetch'i) ve sessiz hücre yeniden-diff'i, hareket edenlerle orantılı
işe + O(N) ucuz taramaya indirgendi (aşağıdaki dürüst not). (2) "Yazar
işareti unuttu" disiplin sınıfı — yapısal olarak kapatıldı: işaret bevy'nin
yazma yolunun kendisinde; unutulacak bir `bump()` yok. Bu, `WireId`
minting kapısı (tip düzeyinde tek inşaat yolu) ile aynı yapısal garantı
sınıfıdır — projenin üç kez yaralandığı "yorumda disiplin"in kodda
karşılığı.

**Dürüst not — "∝ movers"un sınırı (spec'e açık beyan):** (a) bevy'nin
`Changed<T>` filtresi `IS_ARCHETYPAL = false` (bevy_ecs 0.19.1 kaynağı
doğrulandı): tarama, eşleşen tablodaki **her** entity için change tick
karşılaştırması yapar — yani `update`'in taraması O(N)'dir (ucuz:
entity başına bir u32 karşılaştırma; eski rebuild'in hücre hesabı + hash
yazımlarından ~5× ucuz, ama O(movers) DEĞİL). O(movers) olan kısım
gerçek iş: bucket/defter güncellemeleri, doğum aritmetiği, kodlama.
(b) Tick başına **hareketsizlikten bağımsız bir taban** kalıyor ve B bunu
kaldıramaz: bağlantı-bazlı `private` frame'leri (ack — §14.1,
bağlantı-bazlı fan-out tasarımı gereği) ~92–106 µs/tick + core fazlar
(CONTROL drain, READ pull, CONVERT ingest, 4b grup tablosu, 4d fan-out
`try_send`, metrik) ~177–208 µs/tick ≈ **270–310 µs/tick taban**.
In-proc modda bu tabana 500 eş-konumlu istemci task'ının işi de eklenir
(aşağıdaki bimodal bulguyla bağlantılı).

**Elenen alternatifler.** (1) *El yapımı dirty hunisi* (`Position`'a dirty
bayrağı bileşeni / ayrı dirty kümesi; `With<Dirty>` archetype filtresiyle
O(movers) tarama) — tarama O(movers) olurdu AMA işareti **yazarların
kurmaya zorunlu** olması gerekir: bu, kapanan kusur sınıfının ta kendisi
(üç kez yaralanılan "unutma" disiplini) ve yapısal olarak kapatılamaz —
her yeni yazan (yeni sistem, yeni ingest yolu, test helper) denetim
istiyor. O(movers) tarama kazancı (~20–40 µs/tick, ölçülen) yapısal
garanti karşılığında verilmedi. (2) *Saf disiplin* (yazara oda metodunu
çağırma zorunluluğu + yorum + lint) — lint geleceğin yazanını göremez;
tarih tekerrürü. (3) *bevy event/observer API* — §7'nin orijinal elenme
gerekçesi (0.19 yeniden tasarımı) + hot path event tamponu; standalone'da
event scheduler'ı zaten yok. (4) *Statüko* — üç O(N) tarama + yeniden-diff.

### Ölçüm (before/after nasıl kuruldu, ham veri, acceptance)

**Kurulum (öncesi/sonrasi karşılaştırmasının yapısı).** Aynı makine
(AMD Ryzen 9 7950X 16C/32T, 124 GiB; masaüstü yükü — run başına loadavg
meta dosyalarında), aynı komut (`gsb-loadgen 500 --duration 30
--profile still --still-frac F --visibility spatial --cell-size 20`,
in-proc: 500 istemci + 1 oda tek process, tokio worker=1), **release**
profil (lto=thin, codegen-units=1), rustc 1.95.0. Üç kod durumu (BASE =
`c7366b1`, A = BASE + madde A, B = BASE + A + B — teslim edilen durum)
üzer üste **sıralı** derlendi; her run başka bir cargo çalışmasıyla
eşleşmedi (kontaminasyon kuralı). Yerel BASE yeniden ölçümü spec'in
dışarıdan verdiği 782 sayısını birebir yeniden üretti (782-bin ×4,
pinned ve unpinned). **Pinned set:** `taskset -c 8-15` (ölçüm sırasında
~%90+ boş çekirdekler; unpinned koşular ayrı raporlandı ve kontamine
olanlar "kanıt değil" olarak işaretlendi).

**Adım-zamanı histogramı ve "391/782" etiketleri (okuma uyarısı).**
RESULT satırındaki `step_p50_us=391/782` **kesin quantil DEĞİLDİR**:
metrik histogramı log2 binlidir (bütçe 33 333 µs: bin0 [0,260), bin1
[260,521), bin2 [521,1042), bin3 [1042,2083) µs); "391" = p50'nin bin1'de
olduğunun, "782" = bin2'de olduğunun temsilcisidir (ortalar
≈(260+521)/2, ≈(521+1042)/2). Kesin karşılaştırma `step_mean_us`'dir
("server room (final)" satırı, ham dosyalarda).

**Bimodal bulgu (önemli, koşu koşu tekrarlanabilir):** adım zamanları her
kod durumunda **iki keskin küme** oluşturuyor — komşu log2 binleri,
tam 2.0 oran (bimodalite frekans artifact'ı DEĞİL: pinned koşu sırasında
/proc/cpuinfo örneklemesi çekirdeklerin ~5.4 GHz'de sabit kaldığını
gösterdi). Kanıtlar: (i) her koşunun histogramı iki binde keskin
yığılma (aşağıdaki tablo); (ii) **aynı binary, aynı pinned çekirdekler,
aynı frac, farklı koşu** → farklı küme karışımı (B 0.00: sweep koşusu
p50~391 bin, %70 hızlı adım; probe koşusu p50~782 bin, %3 hızlı adım);
(iii) hızlı küme payı **her durumda stillness ile artıyor** (BASE
%10/3/8/38 → A %37/27/64/75 → B %70/66/80/90; 0.00→0.99). En güçlü
gipotez (gipotez olarak raporlanır, izole edilmedi): in-proc tek-worker
topolojisinde oda'nın working set'inin L2'de oturması/oturmayışı;
bozma basıncı 500 eş-konumlu istemci task'ından (tik başına işleri
taşıdıkları frame sayısıyla ∝ aktivite). Mekanizmanın tam izolasyonu bu
turun kapsamı dışında bırakıldı (aşağı "yapılmayanlar").

**Karşılaştırılabilir (pinned) adım ortalamaları, µs/tick**
(`step_mean_us`; p50-bin = adım p50'nin düştüğü log2 bininin temsilcisi;
hızlı% = bin0+bin1 adım payı):

| durum | still_frac 0.00 | 0.50 | 0.90 | 0.99 |
|---|---|---|---|---|
| BASE mean | 712.2 (bin782) | 753.3 (bin782) | 720.1 (bin782) | 548.2 (bin782) |
| BASE hızlı% | 10 | 3 | 8 | 38 |
| A mean | 578.3 (bin782) | 620.8 (bin782) | 514.6 (**bin391**) | 448.6 (**bin391**) |
| A hızlı% | 37 | 27 | 64 | 75 |
| B mean | 492.6 (**bin391**) | 499.2 (**bin391**) | 433.5 (**bin391**) | 362.3 (**bin391**) |
| B hızlı% | 70 | 66 | 80 | 90 |

Tam histogramlar (bin0,bin1,bin2,bin3…; BASE: [0,91,741,68] /
[0,24,797,79] / [0,75,803,20,1] / [2,339,557,2]; A: [0,337,556,7] /
[0,244,637,19] / [1,570,327,2] / [28,666,205,1]; B: [6,627,264,3] /
[7,588,303,2] / [33,701,161,5] / [161,648,90,1]).

**Faz dökümü (probe ile, µs/tick; 90 tick'lik kararlı pencere; koşu modu
parantezde):**

| faz | BASE 0.90 (yavaş ağırlıklı) | A 0.90 (karışık) | B 0.90 (hızlı ağırlıklı) | B 0.00 (yavaş koşu) |
|---|---|---|---|---|
| sys (sistemler) | 14 | 13 | 14 | 26 |
| stamp (yetim damga) | 2 | 1.5 | 1.7 | 2 |
| rebuild / dirty (update) | **97** | **76** | **32** | **115** |
| group_of | 28.5 | 28 | 47 | 30 |
| snap (parça+bileşim) | **254** | **53** | **26** | 23 |
| ka | 1.7 | 1.6 | 2 | 1 |
| priv (private frame'ler) | 106 | 105 | 92 | 120 |
| (core — ölçülmeyen fazlar) | ~208 | ~278* | ~177 | ~350* |

*modal karışım oranlarıyla çarpılmış tahmini. Satır toplamı ≈ koşunun
`step_mean_us`'i. Dikkat: sys/stamp/rebuild scope'ları iç içe (gölgeli
bağlayıcı) — tablodaki değerler türetilmiş gerçek faz değerleridir
(sys_total − stamp_total vs. stamp_total − rebuild_total).

Okunuş: A'nın el ettiği faz **snap** (254→53: yeniden-diff'in memo'su).
B'nin el ettiği faz **rebuild** (97→32: O(N) rebuild+doğum taraması →
değişim listesi + O(N) ucuz tarama). Dürüst not: `last_cell` tablosu
`group_of`'u **ucuzlaştırmadı** (B 47 µs vs BASE ~15–28 µs eşdeğer —
iki hash tablosu lookup'u ≈ bevy'nin sıkışık tablo fetch'i; beklenen
kazancın gerçekleşmediği faz, raporda saklanmıyor). B'nin net kazancı
update fazında + A'nın snap kazancının üstüne birikmede (snap 26: diff
yok, liste kodlanır).

**Acceptance (spec: "süpürme artık düz olmamalı; DÜZ KALIYORSA B İŞE
YARAMAMIDIR — BUNU AÇIKÇA SÖYLE"):**
- **B süpürmesi düz DEĞİL** — ama nerede eğimli olduğu açıkça söylenir:
  p50 **bin** düzeyinde B'nin dört noktası aynı binde (391-bin;
  BASE'in dört noktası 782-bin'deydi — B'nin ortanca adımı HER frac'te
  2× daha düşük binned). Eğim `step_mean`'de: 492.6 → 499.2 → 433.5 →
  362.3 (0.00→0.99: **−130.3 µs, −%26.4**; 0.99, 0.50'nin de %26.5 altı)
  ve mod karışımında: hızlı adım payı %70→%90.
- Eğimin sınırlı olmasının (derin bir eğri olmamasının) nedeni yukarıda:
  **270–310 µs/tick'lik hareketsizlikten bağımsız taban** (priv + core
  fan-out + in-proc istemci task'ları) B tarafından yapısal olarak
  kaldırılamaz (§14.1 bağlantı-bazlı fan-out). Bu taban olmasa B'nin
  0.99 adımı ~100 µs bandına inerdi; taban varken ~362 µs.
- **Spec'in istediği rapor: adım zamanı farkı 0.99 vs 0.00:** B: 362.3 vs
  492.6 µs (−130.3 µs / −%26.4); BASE: 548.2 vs 712.2 µs (−164.0 µs /
  −%23.0 — BASE'in kendi eğimi kısmen mod karışımı şansına bağlı, not
  edildi).
- Unpinned koşular (BASE/A/B; loadavg 4–11, B 0.90/0.99 koşusu load
  spike'ı yedi: 11.7) **kanıt değil** olarak işaretlenir; kontrol
  karşılaştırması pinned settir. Unpinned BASE yine 782-bin×4 (spec'in
  dış veriyle tutarlı), unpinned A 782/782/391/391-bin (A'nın
  eğiliminin ilk kanıtı), unpinned B 782/782/782/391-bin (0.90/0.99
  kontamine).

**Ham RESULT satırları (pinned set, kırpılmamış, 12 koşu; loadavg:
meta dosyaları — BASE 8.5–9.4, A 6.5–10.1, B 4.3–5.7; binary sha256'lar
aynı dosyalarda):**

```
--- BASE (c7366b1), still_frac=0.00 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=447745 snap_per_client_p50=897.0 tick_hz_med=30.00 client_in_bps=23695677 client_out_bps=36892 out_bps_per_conn=47239 moves=89380 errors=0 steps=900 server_hz=30.00 step_p50_us=782 step_max_us=1864 step_over_budget_pct=0.0 dropped=0 late_max_us=663 peak_payload_b=4280 snap_overflows=2162 records_per_tick=192.5 overlap_x=0.39 server_in_bps=24774 server_out_bps=23619660 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=89306 ack_processed_max=180 ack_lag_max_ms=115 fulls=33092 private_fulls=18474 deltas=432762 gap_drops=365 view_size=250000 still_frac=0
--- BASE (c7366b1), still_frac=0.50 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=444774 snap_per_client_p50=891.0 tick_hz_med=30.00 client_in_bps=10248223 client_out_bps=18693 out_bps_per_conn=20297 moves=44750 errors=0 steps=900 server_hz=30.00 step_p50_us=782 step_max_us=1842 step_over_budget_pct=0.0 dropped=0 late_max_us=2044 peak_payload_b=3262 snap_overflows=342 records_per_tick=112.1 overlap_x=0.22 server_in_bps=12526 server_out_bps=10148312 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=44750 ack_processed_max=178 ack_lag_max_ms=108 fulls=24785 private_fulls=10103 deltas=429731 gap_drops=361 view_size=136165 still_frac=0.5
--- BASE (c7366b1), still_frac=0.90 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=444356 snap_per_client_p50=890.0 tick_hz_med=30.00 client_in_bps=3650051 client_out_bps=4267 out_bps_per_conn=7117 moves=9350 errors=0 steps=900 server_hz=30.00 step_p50_us=782 step_max_us=34685 step_over_budget_pct=0.1 dropped=0 late_max_us=2080 peak_payload_b=2721 snap_overflows=354 records_per_tick=46.9 overlap_x=0.09 server_in_bps=2821 server_out_bps=3558531 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=9350 ack_processed_max=178 ack_lag_max_ms=103 fulls=18189 private_fulls=3578 deltas=429379 gap_drops=366 view_size=84122 still_frac=0.9
--- BASE (c7366b1), still_frac=0.99 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=314522 snap_per_client_p50=588.0 tick_hz_med=30.00 client_in_bps=2709956 client_out_bps=1016 out_bps_per_conn=5295 moves=1370 errors=0 steps=900 server_hz=30.00 step_p50_us=782 step_max_us=1126 step_over_budget_pct=0.0 dropped=0 late_max_us=1562 peak_payload_b=2769 snap_overflows=381 records_per_tick=49.0 overlap_x=0.10 server_in_bps=633 server_out_bps=2647321 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=1370 ack_processed_max=175 ack_lag_max_ms=70 fulls=16661 private_fulls=1970 deltas=299464 gap_drops=367 view_size=77262 still_frac=0.99
--- A (BASE + madde A), still_frac=0.00 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=444115 snap_per_client_p50=890.0 tick_hz_med=30.00 client_in_bps=23504358 client_out_bps=36611 out_bps_per_conn=46789 moves=88738 errors=0 steps=900 server_hz=30.00 step_p50_us=782 step_max_us=1425 step_over_budget_pct=0.0 dropped=0 late_max_us=2233 peak_payload_b=4280 snap_overflows=2151 records_per_tick=192.7 overlap_x=0.39 server_in_bps=24580 server_out_bps=23394361 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=88715 ack_processed_max=178 ack_lag_max_ms=113 fulls=32760 private_fulls=18196 deltas=429154 gap_drops=397 view_size=250000 still_frac=0
--- A (BASE + madde A), still_frac=0.50 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=447918 snap_per_client_p50=897.0 tick_hz_med=30.00 client_in_bps=10261605 client_out_bps=18801 out_bps_per_conn=20322 moves=45000 errors=0 steps=900 server_hz=30.00 step_p50_us=782 step_max_us=1373 step_over_budget_pct=0.0 dropped=0 late_max_us=2072 peak_payload_b=3272 snap_overflows=341 records_per_tick=111.3 overlap_x=0.22 server_in_bps=12601 server_out_bps=10161010 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=45000 ack_processed_max=179 ack_lag_max_ms=107 fulls=24898 private_fulls=10263 deltas=432875 gap_drops=408 view_size=134922 still_frac=0.5
--- A (BASE + madde A), still_frac=0.90 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=447335 snap_per_client_p50=896.0 tick_hz_med=30.00 client_in_bps=3631399 client_out_bps=4289 out_bps_per_conn=7078 moves=9400 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_max_us=1590 step_over_budget_pct=0.0 dropped=0 late_max_us=1804 peak_payload_b=2711 snap_overflows=351 records_per_tick=46.1 overlap_x=0.09 server_in_bps=2835 server_out_bps=3539233 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=9400 ack_processed_max=179 ack_lag_max_ms=102 fulls=18241 private_fulls=3570 deltas=432299 gap_drops=365 view_size=84055 still_frac=0.9
--- A (BASE + madde A), still_frac=0.99 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=318802 snap_per_client_p50=664.0 tick_hz_med=30.00 client_in_bps=2623139 client_out_bps=1011 out_bps_per_conn=5117 moves=1360 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_max_us=1079 step_over_budget_pct=0.0 dropped=0 late_max_us=1859 peak_payload_b=2691 snap_overflows=364 records_per_tick=43.6 overlap_x=0.09 server_in_bps=630 server_out_bps=2558594 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=1360 ack_processed_max=174 ack_lag_max_ms=68 fulls=16648 private_fulls=1938 deltas=303725 gap_drops=367 view_size=77452 still_frac=0.99
--- B (teslim durumu, BASE + A + B), still_frac=0.00 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=444282 snap_per_client_p50=891.0 tick_hz_med=30.00 client_in_bps=23525987 client_out_bps=36625 out_bps_per_conn=46832 moves=88775 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_max_us=1185 step_over_budget_pct=0.0 dropped=0 late_max_us=1603 peak_payload_b=4274 snap_overflows=2162 records_per_tick=193.8 overlap_x=0.39 server_in_bps=24589 server_out_bps=23416100 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=88772 ack_processed_max=178 ack_lag_max_ms=110 fulls=32838 private_fulls=18167 deltas=429218 gap_drops=393 view_size=250000 still_frac=0
--- B (teslim durumu, BASE + A + B), still_frac=0.50 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=445050 snap_per_client_p50=891.0 tick_hz_med=30.00 client_in_bps=10225700 client_out_bps=18684 out_bps_per_conn=20252 moves=44725 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_max_us=1235 step_over_budget_pct=0.0 dropped=0 late_max_us=1701 peak_payload_b=3188 snap_overflows=344 records_per_tick=111.5 overlap_x=0.22 server_in_bps=12520 server_out_bps=10125771 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=44724 ack_processed_max=178 ack_lag_max_ms=104 fulls=24592 private_fulls=9953 deltas=430044 gap_drops=367 view_size=133974 still_frac=0.5
--- B (teslim durumu, BASE + A + B), still_frac=0.90 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=447297 snap_per_client_p50=897.0 tick_hz_med=30.00 client_in_bps=3735284 client_out_bps=4289 out_bps_per_conn=7286 moves=9400 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_max_us=1254 step_over_budget_pct=0.0 dropped=0 late_max_us=1870 peak_payload_b=2796 snap_overflows=359 records_per_tick=46.7 overlap_x=0.09 server_in_bps=2836 server_out_bps=3643148 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=9400 ack_processed_max=179 ack_lag_max_ms=104 fulls=18144 private_fulls=3503 deltas=432250 gap_drops=406 view_size=84327 still_frac=0.9
--- B (teslim durumu, BASE + A + B), still_frac=0.99 (30 sn, 900 adım, taskset -c 8-15)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=330070 snap_per_client_p50=689.0 tick_hz_med=30.00 client_in_bps=2688098 client_out_bps=1018 out_bps_per_conn=5252 moves=1375 errors=0 steps=900 server_hz=30.00 step_p50_us=391 step_max_us=1288 step_over_budget_pct=0.0 dropped=0 late_max_us=1538 peak_payload_b=2691 snap_overflows=382 records_per_tick=43.7 overlap_x=0.09 server_in_bps=635 server_out_bps=2625834 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=1375 ack_processed_max=176 ack_lag_max_ms=68 fulls=16731 private_fulls=2055 deltas=314962 gap_drops=432 view_size=77635 still_frac=0.99
```

### Tur özeti (yeni testler, hiçbir eski test silinmedi/ihmal edilmedi)

- `gsb-game` lib (aoi) — **9 yeni** test:
  - Spec'in gerekli 3 A (cache-invalidation) testi:
    `aoi_tick_cache_no_stale_block` (art arda iki tick, farklı içerik →
    farklı blok; bayat parça yeniden sunulmaz),
    `aoi_silent_delta_silent_no_stale` (Silent→Delta→Silent: bayat
    Delta yeniden sunulmaz; sessizlik bayt taşımaz; keepalive full
    GÜNCEL içeriği taşır),
    `aoi_two_groups_same_cell_same_block` (iki grup, aynı hücre, aynı
    tick → içerik-benzeri blok + `encoded_records()==1` — bir kez
    kodlandı, referansla paylaşıldı).
  - B testleri: `aoi_structural_dirty_direct_write` (odanın HİÇ hook'u
    olmayan bir yazan — doğrudan `world.entity_mut` — doğru delta
    üretir: işaret yapısal), `aoi_partial_delta_only_mover_recorded`
    (5 kayıt içeren hücrede yalnızca 1 kaydın deltası; diğer 4
    yeniden kodlanmaz), `aoi_quantized_move_no_wire_change_no_record`
    (wire pozisyonu değişmeyen hareket → kayıt yok, hücre Silent),
    `aoi_leave_removal_in_delta_and_cell_exit` (despawn yolu: `removed`
    + boşalan hücrede tek `CellExit`), `aoi_member_join_npc_cell_born_full`
    (spec-(b) sapma durumu: NPC-only hücreye üye katılır → doğum; üye
    grubun full'uyla baselined, private atlanır),
    `aoi_join_leave_same_tick_inert` (aynı tick'te join+leave: park yok,
    sayım/bucket sağlam).
- Eski testlerin hepsi koştu: **111 → 120** (120/120 yeşil + 1 var olan
  `#[ignore]`'li gsb-lint doctest; 0 silinen, 0 ihmal edilen). 6 kilitli
  delta testi (`tests/delta_aoi.rs`) **değişmeden** yeşil. `cargo
  clippy --workspace --all-targets` temiz (0 uyarı).
- **Yapılmayanlar (açık beyan):** (1) 270–310 µs/tick'lik
  bağlantı-bazlı taban (priv + core fan-out) indirilmedi — B yapısal
  olarak kaldıramaz; bu bir sonraki turun konusu (kanal batch'i /
  worker topolojisi). (2) 2× bimodalitenin mikromimari mekanizması
  izole edilmedi (frekans elendi; L2 oturumu gipotezi olarak
  raporlandı). (3) 491 hücreli spread probe'u yeniden koşulmadı. (4)
  Probe altyapısı (`GSB_PHASE`) tur sonunda **kaldırıldı** (geçiciydi;
  commit'te yok); geçici probe dosyası `tests/zzz_probe_bevey.rs`
  silindi (change penceresinin 6 senaryosu bu turda doğrulandı:
  baseline-öncesi spawn, sistem yazımı + değişmeyen yazım, sessizlik,
  update-arası insert, update-arası spawn, aynı-değer yeniden yazım).

---


## Kapatılanlar (delta yayın + input sıralama turu)

### A — Input sıralama + onay (oyun bandında seq/ack)

**Ne.** İstemci girdileri artık **oturum başına numaralıdır**
(`MoveTo.seq`, `uint64`, 1'den başlar); oda, bağlantı başına iki u64'lik
`InputState{hwm, acked}` tutar (yüksek su); sunucu, işlediği son sırayı
bağlantının **private** frame'inde onaylar (`Private{ack: InputAck |
snapshot: WorldSnapshot}` oneof'u, op 1004). Kural: `seq > hwm` → işle +
`hwm = seq`; `seq ≤ hwm` → **sessizce at** (normal yarış — geç/dupl girdi —
protokol ihlali değil, bütçe harcamaz); boşluk işareti engellemez
(yüksek-su, ardışıklık değil); `seq = 0` = numaralandırma öncesi girdi
(işlenir, hwm'yi ilerletmez, asla ack'lenmez — eski istemci geri
uyumluluğu). Rejoin'de **iki taraf sıfırdan** (sunucu `on_join`'da
sıfırlar; yoksa yeni istemci'nin seq 1'i eski hwm'nin altında kalır ve
kalıcı olarak atılırdı). Tick başına bağlantı başına **en fazla bir
private frame**: full gelirse ack bir tick ertelenir (full kazanır).
Test: `tests/input_ack.rs` (4): **ack monoton ilerler ve sunucunun
işlediğinden fazlasını asla ack'lemez** (spec'in gerekli testi,
gittikçe büyüyerek 5 numaralı girdiyle doğrulanır), dupl girdi entity'yi
geri döndürmez + boşluk işareti engellemez, rejoin oturumu sıfırlar,
numaralandırmasız girdi işlenir ama asla ack'lenmez.

**Kapanan kusur sınıfı.** Dupl/geç girdi entity'yi sessizce **geriye**
taşıyabiliyordu (istemci retry'ı, yeniden bağlanma sırasında kuyruktaki
eski girdi); rejoin'de istemcinin prediction'ı sunucunun neredeyse
uzlaştığını bilecek **mutabakat noktası** yoktu. Onay bir **işleme
işareti**dir (prediction mutabakatı), teslim garantisi değil — teslim
garantisi taşıma katmanındadır (DESIGN §14.2).

**Elenen alternatifler.** (1) *Ardışıklık tabanlı onay* (istemci, boşluk
dolana kadar bekler) — kayıp-toleranslı oyun bandında işareti **kayıba
bloke** eder; "kayıp paket = bir kademe bayatlık" kabulüyle çelişir ve
rUDP oyun bandında girdi kaybı zaten meşrudur. (2) *Taşıma katmanında
(rUDP kontrol bandı) seq/ack* — kontrol bandının işi zaten öyle (AUTH/
JOIN/LEAVE/HEARTBEAT); girdi işareti **oyun** anlamındadır (hangi girdi
dünyada uygulandı?) ve oyun bandının rejoin/loss toleransı içinde
yaşamalıdır; kontrol bandına taşımak istemciye oyun durumunu taşıma
zamanlamasını taşımadan işe yaramaz. (3) *Seq'siz statüko* — kapanan
kusur sınıfı; ayrıca "işlediğinden fazlasını asla ack'lemez" testinin
konusu olan sınıf hiç ifade edilemezdi. (4) *Bağlantı başına input
tamponu + yeniden işleme* — §14.1 ilkesine (bağlantı başına durum yok)
açık ihlal; tampon + zamanlayıcı = ayrı mimari.

### B — Hücre = kodlama birimi + delta (`spatial` stratejisi)

**Ne.** `spatial` stratejisinin snapshot'ları delta kodlandı: kodlama
birimi **grup değil hücredir**. Oda, tick başına tüm hücrelerin
(önceki, güncel) bucket çiftine bakar (iki bucket haritası **rotasyonla**
döner — kopya yok) ve hücre başına bir parça üretir
(`Silent | Exited | Appeared | Delta`), dondurulmuş `Bytes` olarak
önbelleğe alır (tick başına hücre başına **bir kez** kodlama). Grup bir
**kitle**dir: grubun paketi, gördüğü (3×3) hücrelerin parçalarının
birleşimidir — referansla paylaşım (encode-once/share-bytes omurgası
aynen, §14.1). full/delta kararı **(grup, hücre) başınadır**:
yeni doğan grup bir kez full (delta=false); devam eden grup delta
(delta=true). **Delta değişmezi:** bir hücrenin deltası
`içerik(şimdi) vs içerik(önceki tick)` — gruptan bağımsız, saf hücre
fonksiyonu; yerleşik grubun istemcileri her zaman önceki tick'in içerik
ile senkron olduğundan (indüksiyon — DESIGN §8.1) grup-başına defter
gerekmez. Paket içi sıra sabit: `removed` (entity çıkışları) →
`cell_exits` (hücre çıkışları) → `entities` (upsert'ler) — entity iki
görünen hücre arasında geçerken **kaynağındaki hücre** çıkışı raporlar
(hücre-yerel, ucuz); boşalan hücre **tek** `CellExit` kaydıdır
(50 entity'li hücre = 1 kayıt). `delta` bayrağı modu wire'da ayırt
edilir kılar (yanlış-mod istemci sessizce yanlış uygulamaz).

**Geç giriş / grup geçişi:** baseline'ı olmayan yeni grup üyesi bir kez
**private full** alır (aynı batch'te, grubun gap'de bıraktığı delta'dan
hemen sonra); istemci bunu **koşulsuz** uygular (grup akışının ayrı bir
akımı — grup stream'inde seq mantığı dışında); grup o tick'ten sonra
delta'da kalır. Testler: `late_join_sees_full_world_one_shot` +
`late_joiner_receives_full_world_snapshot` (eski, hâlâ canlı) +
`tests/delta_aoi.rs` (6, spec'in gerekli B testleri): **delta akışı ile
full akışı aynı istemci görünümünde yakınsar**; hücre değiştiren entity
**her** istemci pozisyonunda (iki hücreyi de gören / kaynak-tek /
hedef-tek) hayaletsiz + kopyasız (tick-tick değişmezi); hücre grubun
görüşünden çıkınca entity'ler istemci tarafında **gerçekten silinir**
(tek `CellExit`, yeniden taşınmaz); yarıda giren istemci **tek seferlik
full** ile tüm dünyayı görür (grup delta'da kalır — küçük bir hareketin
delta ile taşındığı ispatlanır); delta kaybı **keepalive periyodu
içinde** iyileşir (ölçülen sınır: kayıp bitimi + 31 tick; iyileşen
paketin TAZE full olduğu doğrulanır); paket modu wire'da kendi kendini
tanıtır (aktif + sessiz grupta iki mod da görülür). `tests/aoi.rs`
(eski "son snapshot'ın id seti" gözlemi) delta stream'inde geçerli
olmadığından **istemci-VIEW gözlemine** yazıldı (aynı amaçlar: fanout /
hücre geçişi / kimlik değişmezi).

**Keepalive kararı (spatial):** her keep tick'inde (vars. 30 tick) her
grup — aktif olsun ya da sessiz — **taze full** gönderir; aktif grupta
o tick'in delta'sının yerine geçer. *Elenen:* (a) *son paketi yeniden
gönder* (eski full-snapshot davranışı) — delta modda anlamsız: delta
uygulanmadıysa bayattır, uygulandıysa bilgi taşımaz; kayıp iyileşmez.
(b) *keepalive'sız* — delta kaybı sınırsız bayatlık; resync istemi ayrı
bir protokol (istemci-sunucu istek/yani await — oda tick gövdesine
await eklenemez). (c) *sadece sessiz gruplara full* — aktif gruptaki
istemciler de kayıp yaşar (kayıp grubun aktifliğine bağlı değildir).

**Sis güvenlik parametresi:** mahalle **içerikten bağımsız** 3×3'tür —
içerik bazlı mahalle ("komşu hücrede entity yoksa görünmez") gizlilik
kararını dünya durumuna bağlar ve "kim görünüyor"u içerikle karıştırır.
Tek güvenlik düğmesi `cell_size`: hücre büyüdükçe mahalledeki
potansiyel entity sayısı artar (küçük = sıkı sis). **Oyuncu-bazlı
aydınlatılmış hücre saklaması** (aynı hücrede bile ışık konisi) bu turun
kapsamı dışında — bu maddenin devamı olarak P2'ye alındı (aşağı).

### C — Wire kuantizasyonu bulgusu + `still` profili + ölçüm

**Bulgu (ölçüm tuzağı + gerçek kusur).** Ring/spread profillerinde her
entity her tick hareket eder — delta'nın kazancı bu profillerde
görünmez (spec uyarısı; ölçüm için **üçüncü profil** eklendi). Daha
kötüsü: hareket 10 u/sn, 30 Hz → tick başına 1/3 wire birimi; wire
konumlar i32 (kesik) → hareket eden entity bile wire konumunu her ~3
tick'te bir değiştirir → grup stream'i **sıra-düzgün değildir** (seq =
oda tick'i; frame yalnızca değişimde). İlk istemci kuralı ("gap'te atla,
tam gelene kadar") bu yüzünden **dejeneratif**ti: kayıp testi, sıfır
paket kaybıyla delta'ların ~2/3'ünü "gap" düşürüyordu — istemci her
sessizlikten sonra 1 Hz full'ı bekliyordu, delta yolu işlevsizdi.

**Çözüm (istemci kuralı):** seq boşluğu **kayıp kanıtı değil, normal
durum** — akış olay-odaktır. İstemci baseline'ı olan delta'yı boşluğa
rağmen **üzerine uygular** (kayıtlar mutlak + idempotent: konum
upsert'i, mutlak id ile unutma, mutlak hücre ile unutma; bayat view
üzerinde güvenli — en kötü hal 1 keepalive periyoduna kadar bayatlık,
kaçırılan çıkış geçici hayalet olabilir); yalnızca baseline'ı **olmayan**
delta atılır (one-shot private full aynı batch'te iyileştirir).
**Yakınsama garantisi keepalive full'ıdır** (bir periyotta tam durum),
gap kuralı değil. *Elenen:* (a) *gap'te atla* (ilk uygulama — yukarıdaki
dejenerasyon); (b) *seq = grup yayın sayacı* (boşlukları kaldırır gibi
görünür ama loadgen `tick_hz_med`'ın (son_seq−ilk_seq)/Δt tanımını —
seq = global tick indeksi kuralını — bozardı; saat ölçümü
snapshot akışından gelir).

**Üçüncü yük profili (`still`).** Kayıtların çoğunun **hareketsiz**
olduğu dünya: `--profile still --still-frac F` (vars. 0.9) — id'nin
**sınıflık basamağı** deterministik split (1% granüler, stagger'la aynı
id-bazlı determinizm, her N'de oran tutar): still istemciler tam **bir**
`MOVE_TO` gönderir (ring hedefine oturur, sonra sessiz), azınlık ring
hedefini kovalar (eski `Ring` şekli). `ring`/`spread` **değişmedi** —
orijinal ölçüm tabanı aynen geçerli.

**Ölçülen (N=500, 30 sn, `--visibility spatial`, in-proc; eski =
`f467aff` + minimal still profili yaması [yalnız istemci hareket
şekli — eski wire'da seq yok], yeni = bu commit):** makine 32 thread,
AMD Ryzen 9 7950X 16C, 124 GiB, cargo/rustc 1.95.0, **debug** profil,
in-proc (istemciler sunucuyla CPU paylaşır). Ham RESULT satırları:

```
# ESKİ, still --still-frac 0.90 (f467aff + still yaması)
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=446300 snap_per_client_p50=895.0 tick_hz_med=29.98 client_in_bps=22138489 client_out_bps=3533 out_bps_per_conn=44131 moves=9274 errors=0 steps=900 server_hz=30.00 step_p50_us=3126 step_max_us=7842 step_over_budget_pct=0.0 dropped=45 late_max_us=15633 peak_payload_b=2690 snap_overflows=10513 records_per_tick=2884.5 overlap_x=5.77 server_in_bps=2097 server_out_bps=22065275 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0
# YENİ, still --still-frac 0.90
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=444737 snap_per_client_p50=894.0 tick_hz_med=30.00 client_in_bps=3696869 client_out_bps=4269 out_bps_per_conn=7218 moves=9355 errors=0 steps=900 server_hz=30.01 step_p50_us=6250 step_max_us=8527 step_over_budget_pct=0.0 dropped=120 late_max_us=6164 peak_payload_b=2761 snap_overflows=337 records_per_tick=43.1 overlap_x=0.09 server_in_bps=2822 server_out_bps=3609102 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=9335 ack_processed_max=179 ack_lag_max_ms=118 fulls=18121 private_fulls=3603 deltas=429809 gap_drops=410 view_size=83657 still_frac=0.9
# ESKİ, still --still-frac 0.95
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=442315 snap_per_client_p50=893.0 tick_hz_med=29.99 client_in_bps=21459174 client_out_bps=2088 out_bps_per_conn=42794 moves=4908 errors=0 steps=900 server_hz=30.00 step_p50_us=3126 step_max_us=5992 step_over_budget_pct=0.0 dropped=82 late_max_us=16200 peak_payload_b=2692 snap_overflows=10503 records_per_tick=2791.5 overlap_x=5.58 server_in_bps=1233 server_out_bps=21397061 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0
# YENİ, still --still-frac 0.95
RESULT mode=in-proc visibility=spatial shards=1 max_snap_bytes=1400 clients=500 connected=500 joined=500 left=500 snap_total=438368 snap_per_client_p50=889.0 tick_hz_med=30.00 client_in_bps=3108948 client_out_bps=2473 out_bps_per_conn=6046 moves=4944 errors=0 steps=900 server_hz=30.00 step_p50_us=6250 step_max_us=8411 step_over_budget_pct=0.0 dropped=88 late_max_us=11482 peak_payload_b=2678 snap_overflows=354 records_per_tick=36.2 overlap_x=0.07 server_in_bps=1613 server_out_bps=3023094 peak_conns=500 metrics_dropped=0 profile=still offset=0 procs=1 server_pid=0 client_pids=0 affinity=none server_cpu_s=0.0 clients_cpu_s=0.0 join_rejected=0 cap_rejected=0 budget_rejected=0 actions_dropped=0 actions_dropped_top= transport=tcp retrans_out=0 dup_in=0 oob_dropped=0 gave_up=0 acks=4939 ack_processed_max=179 ack_lag_max_ms=105 fulls=17324 private_fulls=2695 deltas=423373 gap_drops=366 view_size=80103 still_frac=0.95
```

Özet (profili adı: `still`; oran = `still-frac`):

| still_frac | kayıt/tick (eski→yeni) | bant/conn (eski→yeni) | adım p50 (eski→yeni) | adım max | bütçe aşımı |
|---|---|---|---|---|---|
| 0.90 | 2 884 → **43** (**67×**) | 44 131 → **7 218** B/sn (**6.1×**) | 3 126 → 6 250 µs | 7 842 / 8 527 µs | %0 / %0 |
| 0.95 | 2 791 → **36** (**77×**) | 42 794 → **6 046** B/sn (**7.1×**) | 3 126 → 6 250 µs | 5 992 / 8 411 µs | %0 / %0 |

**Kazancın hareketsizlik oranına göre fonksiyonu:** eski yol oranla
yaklaşık sabit (tam 3×3 her tick yeniden kodlanır: 2 884 → 2 791
kayıt/tick, %3) — delta yolu oranın kendisiyle ölçeklenir
(43.1 → 36.2, %16): **hareketsizlik arttıkça kazanç büyür** (oran 1.0'da
teorik sınır = keepalive full'ları + doğum full'ları; kayıt/tick → ~0).
Dürüst takas: yeni yolun tick gövdesi hücre başına fark taraması yapıyor
(bucket rotasyonu + (önceki, güncel) sınıflandırması + parça
önbelleği) — adım p50 ~2× (3 126 → 6 250 µs), her iki versiyonda da
bütçenin %19'undan az; kodlama ~1/67–1/77 + bant ~6–7×. İstemci tarafı
yeni sayaçları: `fulls`/`private_fulls` (private-full sıklığı: 0.90'da
500 istemci × 30 sn'de 3 603 = istemci başına ~7 — warmup'taki grup
geçişleri; still steady-state'te sıfıra gider), `deltas`, `gap_drops`
(baseline'sız atımlar — geç giriş/crossing'lerdeki grup delta'ları;
paket kaybı göstergesi **değildir**, bkz. C kuralı), `acks` /
`ack_processed_max` / `ack_lag_max_ms` (input onayı), `view_size`
(istemci VIEW'larının toplam entity adedi — yakınsama gözlemi).
`ring`/`spread` (her entity her tick hareket): kazanç **yok** —
orijinal taban aynen (önceki ölçümler bu commit'te yeniden alınamaz
durumda değil — profiller değişmedi).

### Tur özeti (yeni testler, hiçbir eski test silinmedi/ihmal edilmedi)

- `tests/input_ack.rs` — 4 yeni (A maddesi).
- `tests/delta_aoi.rs` — 6 yeni (B maddesi; spec'in gerekli B testleri).
- `tests/aoi.rs` — delta stream'inde geçersiz kalan "son snapshot id
  seti" gözlemi **istemci-VIEW gözlemine** yazıldı (aynı amaçlar,
  aynı aslar: fanout/hücre geçişi/kimlik değişmezi).
- `gsb-game` lib içi AOI testleri delta davranışıyla uyumlu hale
  getirildi (private full'ın wire format düzeltmesi dahil — eski ham
  snapshot baytları yerine `Private{snapshot}` oneof sarmalayıcısı).
- Eski testlerin hepsi koştu: 97 → **111** (111/111 yeşil + 1 var
  olan `#[ignore]`'li gsb-lint doctest). `cargo clippy --workspace
  --all-targets` temiz.

---


## Kapatılanlar (koruma katmanı turu)

Bu tur, spec'in garantisini kurar: **sunucu, istemcilerinin yaptığı
hiçbir şeyden dolayı çökmemeli veya kaynak sızdırmamalı.**
Üç hat + doküman (DESIGN §14): (1) oturuş yaşam döngüsü
(yarım açık TCP), (2) kapasite (oda + sunucu geneli), (3) girdi
adaleti (flooding atfesi), (4) kapsam dokümanı. Test: 60 →
**72** (+12: 3 gsb-net + 4 gsb-core + 5 gsb-server e2e; hiçbir eski
test silinmedi/ihmal edilmedi — yeni `Result` join yanıtı
tipine uyan join yardımcılarının imzası güncellendi,
mantiğ/sağlama korunmuş). Makine: 32 core / 124 GB /
rustc 1.95.0 (koşular release).

- [x] **MADDE 1 — Oturuş yaşam döngüsü: yarım açık TCP
  tespit edilip kapatılır (konfigure edilebilir timeout)**
  — Sorun: kablo çekilen/gücü kesilen istemci (FIN/RST gelmez)
  reader pump'ta sonsuza kadar beklerdi; her böyle oturum 3 görev +
  2 kanal + 1 registry kaydı pinlerdi (100k hedefinde sızma).
  **Saati kim tutacak?** Saat, **reader pump'un read deadline'
  ındadır**: her `stream.next()` `tokio::time::timeout(t, …)`
  ile sarılır; **her** istemci frame'i (HEARTBEAT dahil) pencereyi
  sıfırlar; zaman aşımında pump `ConnIn::ServerClosed {
  reason }` yollar, connection actor ERROR 9 gönderip cleanup
  kaskadından çıkar. Bu, bağlantı yolundaki **tek** saattir:
  connection actor'ün tek await'i inbox `recv`'de kalır
  (spec'in sert şartı — `select!` yok), yeni görev / registry
  mesajı / ticker aboneliği eklenmez. Yarım açık TCP "socket
  durumu" ile değil, "T süre boyunca frame yok" ile yakalanır.
  Konfig: `idle_timeout_secs` (vars. 30; 0 = kapalı).
  - **100k maliyeti (seçilen):** 0 ekstra görev; ~15 MB
    (100k bekleyen `Sleep` ≈ 150 B); ~1–2 core (yalnızca
    gerçekten boşta olan bağlantılar uyanır; uyanan çıkar).
  - **Elenen alternatifler (100k matematiği):**
    - **(A) Bağlantı başına timer görevi** (watchdog/interval):
      +100k görev ≈ 100–200 MB (tokio görevi ≈ 1–2 KB);
      her istemci frame'inde yeniden arm → 100k bağlantı ×
      30–100 frame/sn ≈ 3–10M timer işlemi/sn ≈ 5–10% core +
      100k+ allokasyon/sn.
    - **(B) Registry'de son-görülme damgası:** her heartbeat →
      registry mesajı = 100k msg/sn server geneli; registry'nin saat
      tutması gerekir (ticker aboneliği) = kontrol düzleminde
      ikinci beklenen kaynak; 100k msg/sn registry, join yolunu aç bırakır.
    - **(C) Bağlantı başına ticker alıcısı:** 100k
      broadcast receiver; her tick 30/sn × 100k slot yazımı = 3M
      slot yazımı/sn ≈ 3–6% core + 100k receiver durumu;
      **ve** connection actor iki kaynaktan beklerdi (inbox + ticker)
      = `select!` — spec ihlali (karar verici neden).
  - Test: `idle_peer_gets_server_closed`,
    `active_peer_resets_the_idle_window` (tempolu poke — kontrol
    edilebilir `mpsc`-arkalılı `Stream` adaptörü; TCP buffer'ı
    tempoyu yuttuğu için), `eof_reports_peer_closed_not_idle`;
    e2e: `idle_connection_is_closed_by_the_server` (ERROR 9, EOF'tan
    önce), `active_heartbeat_survives_the_idle_window` (250 ms
    heartbeat, 1 s pencere, 3+ ACK).

- [x] **MADDE 2 — Kapasite: oda `max_players` + sunucu geneli
  `max_connections` (ölçüme dayalı varsayılanlar)**
  — Sorun: `CoreError::RoomFull` vardı ama öçülmeyen bir
  dallıydı; oda sınırsız büyürdü, sunucu geneli cap
  yoktu. **Varsayılanlar ölçülen sayıya dayandı** (C1:
  ayrı proses, spatial AOI, 30 Hz, tek oda):

  | Oyuncu | p50 (ms) | server_hz | fan-out drop | late (s) |
  |---|---|---|---|---|
  | 5k | 12.5 | ~30 | 9 086 | ~0 |
  | 8k | 25 | ~30 | 45 596 | ~0 |
  | 9k | 25 | ~30 | 30 869 | ~0 |
  | 10k | **50** | **23.2** | 52 771 | **2.17** |

  p50, 33.3 ms bütçesini **9k–10k arasında** aşıyor
  (not: "drop" sütunu fan-out `dropped_frames`'tir — aksiyon
  düşmü değil). → `max_players` = `Some(10_000)` (ölçülen
  duvar), `max_connections` = `Some(100_000)` (DESIGN §1 hedefinin
  guardrail'i).
  - **Reddi semantiği (karar: nazik Error, sessiz kapatma değil):**
    maliyetler farklı. Oda dolu → **ERROR 8, bağlantı yaşar**:
    reddedilen istemci başka odaya join edebilir/bekleyebilir; sessiz
    kapatma = reddedilen istemcilerin reconnect/backoff fırtınası
    (tam istenmeyen DoS amplifikatörü); maliyet = 1 ekstra frame.
    Bağlantı cap'i → **ERROR 9 + hemen kapatma**: reddedilen
    bağlantının yapabileceği hiçbir şey yok — açık tutmak
    sadece oturum pinler; kapatma EOF'tan önce bildirilir (istemci
    ağ hatası olmadığını bilir).
  - **Spec notu (yer çevikliği):** cap, accept loop'ta değil
    **registry'de** uygulanır — sayım, guardrail'in olduğu yerde
    yaşamalıdır; accept loop, ikinci bir beklenen kaynak eklemeden
    kopmalara gözlemleyemez. Reddedilen bağlantı tabloya kaydedilmez;
    JOIN’in `ServerClosed` ile yaraşı `ConnClosed`'da
    işlenir (dispatcher slot'u drenaj edilir).
  - **Kanıt (ham):** 150 istemci, `--max-players 100`, 10 s:
    `RESULT mode=in-proc visibility=all max_snap_bytes=1400
    clients=150 connected=150 joined=100 left=100 … steps=300
    server_hz=30.00 step_p50_us=130 … dropped=0 … peak_conns=150
    … join_rejected=50 cap_rejected=0 actions_dropped=0
    actions_dropped_top=` (— tam 50 reddi, tam 100 join; reddedilen
    istemcilerin sonraki MOVE_TO'larına sunucu ERROR 6
    (NotInRoom) ile karşılık verir — bağlantı sağlıklı kalır).
    100 istemci, `--max-connections 50`, 10 s: `RESULT mode=in-proc
    … clients=100 connected=100 joined=50 left=50 errors=0 …
    server_hz=29.99 … dropped=0 … peak_conns=50 …
    join_rejected=0 cap_rejected=50 actions_dropped=0 actions_dropped_top=`
    (— registry tablosu asla 50'yi aşmaz; TCP katmanı tümünü kabul eder;
    cap oturum seviyesindedir).
  - Test: `join_rejected_when_room_is_full` (reply `CoreError::RoomFull`,
    kalan üyenin girdisi etkilenmez), `spawn_rejected_when_room_is_full`,
    `conn_opened_rejected_at_connection_capacity` (`ServerClosed` içerik
    "capacity"; `ConnClosed` temiz no-op), e2e
    `room_full_returns_gentle_error_code_8` (B hata alır, yaşar,
    heartbeat ACK'lanır), `connection_capacity_rejects_with_code_9`
    (B: ERROR 9 + EOF; A etkilenmez).

- [x] **MADDE 3 — Adalet: flooding bağlantı başkasının
  aksiyonunu evicted edemez; düşen aksiyon atfeli** — Sorun: eski
  READ, tüm bağlantıların aksiyonlarını tek `Vec`'te merge
  ederdi; `max_pending_actions` aşımında `drain(..over)` = **en
  eski** atılırdı: tek bir flooder'ın backlog'u başkasının
  aksiyonlarını atıyordu (hangi bloğun atılacağı
  `HashMap` sırasına bağlıydı — kurban bile keyfiydi) ve
  söz konusu sayaç oda geneliydi (`dropped_actions`), atfe yoktu.
  - **Çözüm: READ, merge değil sınırlı çekmedir**
    (bounded pull): bağlantı başına tick bütçesi
    `max_actions_per_conn_per_tick` (vars. 16 = 30 Hz'de 480 aksiyon/sn
    ≈ ölçülen 6.7/sn/istemcinin 70×'i) + oda çekme
    bütçesi `max_pending_actions` (vars. 65536). Oda **çektiği
    aksiyonu asla atmaz** (`dropped_actions` yapısal 0; alan format
    uyumu için korunur). Tek kayıp noktası = göndericinin **kendi**
    `Action` kanalı doluyken `try_send` Full (cap 256) — connection
    actor bunu **kendi** metrik örneğinde sayar
    (`m_actions_dropped`), raporda `actions_dropped` (net, kümülatif)
    + `actions_dropped_top` (en çok düşürmüş 5 bağlantı,
    `c{id}:sayı`). Hata frame'i yok, koparma yok (koparmak
    reconnect amplifikatörü olurdu). Hasar sınırı: tek
    saldırgan → odaya en çok 16/tick × 30 = 480 aksiyon/sn +
    256'lik kendi buffer'ı.
  - **Kanıt (ham):** 20 istemci, `--flood-id 7`, 15 s (istemci 7 =
    `c8`): `server net (final): bytes_in=6324 KB …
    frames_in=3234645 … actions_dropped=3225454` /
    `server net (final): actions_dropped_top=c8:3225454` /
    `RESULT mode=in-proc … clients=20 connected=20 joined=20 …
    steps=450 server_hz=30.00 step_p50_us=130 step_max_us=114
    step_over_budget_pct=0.0 dropped=0 late_max_us=668 …
    peak_conns=20 … join_rejected=0 cap_rejected=0
    actions_dropped=3225454 actions_dropped_top=c8:3225454`
    (— 3.2M düşen aksiyonun **%100'ü saldırgana atfeli**;
    diğer 19 istemci temiz 30 Hz: bütçe aşımı %0, fan-out
    drop 0, en geç tick gecikmesi 668 µs).
  - Test: `flooder_cannot_evict_other_connections_actions` (kurbanın
    20/20 aksiyonu içerildi — sıra HashMap'in, atfe garantilenir;
    backlog probe'u: tam olarak 8/tick × 21 tick kadar slot açılır
    = kalan backlog flood'un kendi kanalında), e2e
    `flooder_drops_are_attributed_to_the_flooder` (raporda top[0] =
    flooder ve net toplamla eşitleşir).

- [x] **MADDE 4 — Doküman: DESIGN §14 "Bu base neyi
  hedefliyor, neyi hedeflemiyor"** — Grup değişim birimidir
  (fan-out grup başına, bağlantı başına değil; oda
  tarafında bağlantı başına durum yok; connection actor
  kasıtlı ince durum makinesi). seq/ack yok (teslim garantisi
  TCP'nin; snapshot tam durum; kayıp → bir kademe bayatlık, keepalive
  sınırlar; seq/ack = bağlantı başına durum = başka bir
  mimari — kapı rUDP yolunda, §6). Ölçülen tavan **tek
  odadadır** (C1 duvarı 9–10k; oda actor'ün tek iş parçacıklı
  olmasının doğal sonucudur) → ölçekleme
  kaldıracı **oda segmentasyonu** (`max_players` = ölçülen
  duvar). Sığan aileler: battle royale, arena/MOBA, zoneli MMO/AOI,
  instanced dungeon; sığmayanlar: oda sınırı olmayan tek dünya,
  bağlantı başına oturum durumu, lockstep/sıra garantileri.
  Ayrıca: §3'e oturum saati maddesi, §4'e sınırlı çekme
  (bounded pull) tanımı, §5'e ERROR kod tablosu (Unity referansı
  + `base.proto` yorumu), §10 v1 kısıtları tablosu güncellendi,
  §12 metrik tablosuna `actions_dropped`/`actions_dropped_top`,
  `config.example.toml`'a üç yeni anahtar.


## Kapatılanlar (ayrı proses + yayılma profili + takım-state turu)

Bu tur, önceki turun açık bıraktığı üç soruyu kapatır: (1) in-proc yük
sayılarının duvarı sunucu CPU'su muydu, (2) takım sisli kümeli geometride
sıfır kazanç veriyordu — seyrek/gerçek MOBA geometrisinde ne verir,
(3) tasarımın gizli "grup = konum" varsayımı var mı (takım, conn paritesinden
türetildiği için `group_of` world okumuyordu — kanıtı kolaylaştırmıştık,
bu tur kolaylık kaldırıldı). E1/E2 iki küçük hata düzeltmesi. Test: 58 →
**60** (+1 runtime takım-değişim + 1 ayrı-proces duman testi; hiçbir eski
test değiştirilmedi/silinecek/`#[ignore]` yapılmadı).

- [x] **E1 — `gsb-loadgen --help` panik atıyordu** — `-h/--help` artık
  kullanım metni basıp `exit(0)`; bilinmeyen flag hâlâ panic (sessiz
  yok sayma bug'ını maskelerdi) ama mesajda `(try --help)` işareti var.
  Elenen: (a) **clap** — ~150 satırlık tek amaçlı tool için bağımlılık ağırlığı;
  elle parser zaten 15+ flag'i taşıyor, üç modu da aynı; (b) **bilinmeyeni
  sessizce yok say** — E1'in kendisi o sınıfın habercisiydi.

- [x] **E2 — "server room (final)" `hz=0.00` vs RESULT `server_hz=30.00`** —
  kök neden spec'in tahminine yakın ama farklı: son raporun "önceki aralık
  yok" değil, **0 örnekli pencere** olması — oda örnekleme temposu (1/sn,
  A2: tick başına değil, rapor periyodu başına tek örnek) rapor temposuyla
  faz-kilitli değil; kapanışta son rapor 0-örnekli bir pencere kapatabilir
  (Δsteps=0 → hz=0.0). Düzeltme: final satırı tüm koşu ortancasını (RESULT'un
  `server_hz`'iyle aynı sayı) basıyor; `RoomReport::hz` dokümanına not:
  0 örnekli pencerede hz=0.0 "bu pencerede örnek yok" demektir, tüketici
  pozitif pencerelerin ortancasını almalı. Elenen: (a) son raporu önceki
  pencereden doldur — gerçekliği çarpıtırdı (pencere var, örneği yok);
  (b) örnek+rapor temposunu faz-kitle — metrik semantiği (örnek-aralığı
  hızı, A2) bilerek böyle; görüntü sorunu için core değişikliği.

- [x] **A — Ayrı prosesli istemci modu** — in-proc modun duvarı istemci
  decode'ıydı (10k'da ~6 GB/s protobuf decode, sunucuyla çekirdek
  paylaşımı); sunucu adımlarının gerçek duvarını ölçmek için sunucu artık
  **ayrı proses**te koşabiliyor. Çözüm (tek binary, üç mod):
  - `--serve`: sunucu process'i (in-proc'un aynısı, `Config`'ten).
    `--metrics-listen HOST:PORT` verilirse collector'ın **kanal sink'i**
    (in-proc'un aldığı `MetricReport` struct'larının ta kendisi) ikili bir
    TCP akışına basılır: `[u32 magic][u32 len][LE body]`, 1 Hz. Sunucu
    metrik toplama yolu **aynen korunur** — stdout parsing'i yok, log
    formatı API değil. Flag verilmezse `gsb-metric` log'a yazar
    (`gsb-server` davranışı).
  - `--orchestrate N --procs P`: orkestratör sunucuyu (`--serve`) + P
    istemci process'ini (N istemci, `--offset` ile ardışık küresel id;
    stagger ve profilin id-determinizmi tüm koşu boyunca tutarlı)
    doğurur; istemci çocuklarının `CLIENT` satırları + metrik soketini
    tek RESULT'a birleştirir.
  - **Pinning (`--pin`)**: `unsafe` yasak (pre_exec/sistem çağrısı yok)
    → affinity `taskset -c` sarmalayışıyla; /sys topology'den **SMT-farkında**:
    sunucu ilk 8 fiziksel çekirdeği (16 mantıksal), istemciler kalanı
    round-robin. `taskset` yoksa uyarı + pinsiz koşu (izolasyon o zaman
    sadece proses ayrımına dayanır).
  - **CPU muhasebesi**: `/proc/<pid>/stat` (utime+stime, USER_HZ=100),
    250 ms'lik bekleme turlarında örnekleme — `/proc` reap'te yok olduğundan
    t1 = çıkıştan önceki **son canlı** okuma. RESULT'a: `server_cpu_s` /
    `clients_cpu_s` + `affinity` (çekirdek kümeleri) + `server_pid` /
    `client_pids`.
  - **Neyi feda etti:** (1) istemci varış anları bu process'te yok →
    çocuklar ham değerleri (`connect_ms`, önceden hesaplanmış `hz`)
    `CLIENT` satırında basıyor, orkestratör **ham değerleri** birleştiriyor
    (toplamlar + ham değerler üzerinde percentile → tam, "özetlerin
    özeti" değil); (2) orkestratör iki tarafın da ebe'si (sunucuyu da
    o doğuruyor) → "dış sunucu + bu istemciler" senaryosu `--addr` modunda
    kalıyor; (3) pin için `taskset` binary'si şart (yoksa pinsiz koşu);
    (4) metrik raporları için bir loopback ek istasyonu (1 Hz, önemsiz).
  - Elenen: (a) **thread'ler (ayrı tokio runtime'ları)** — CPU izolasyonu
    yok (amaca aykırı); (b) **ayrı istemci binary/crate'i** — aynı
    binary = metrik codec'inin iki ucu aynı derlemede (format sürüklenemez)
    + istemci kodu zaten `run_client`'ta; (c) **`gsb-metric` log satırlarını
    parse et** — spec yasakladı, ayrıca log formatı API değil (tracing
    filtresi değişse parser kırılır).

- [x] **B — İkinci yük profili: `--profile spread`** — eski profil
  (yarıçap-40 hedef halkası, 4 rad/sn) entity'leri merkez bandında
  kümelendiriyordu → görünürlük stratejileri ayrıştırılamıyordu. Yeni
  profil: her istemcinin deterministik bir **evin** var (±`spawn-half`
  haritada uniform ızgara; sunucu spawn'ı aynı dağılımla — `spawn_half_size`
  config'i, vars. 50 → eski arena bit-bit aynı; spread koşuları 1000 =
  2000×2000 geniş harita), evinin çevresinde yarıçap-20, 0.4 rad/sn
  (hedef 8 u/sn < entity 10 u/sn → entity hedefi takip eder, ayak izi ≈ ev).
  Spawn da aynı dağılımla olduğu koşul **tick 1'den istatistiksel durağan**
  (göç transiyantı yok). Profil RESULT'ta (`profile=ring|spread`) ve
  machine satırında. Eski profil **aynen** varsayılan (tüm önceki ölçüm
  tabanı `ring`; süreklilik bozulmadı).
  - **Dağılım gerekçesi:** uniform, tek değişkenli ve tek doğru seçenek:
    C2 iddiası "geniş harita, seyrek görevli" ve uniform'da görüş kapsamı
    yalnız (kaynak, yarıçap, alan) fonksiyonu (`c = 1−exp(−kπr²/A)`) —
    yorumlanabilir. Herhangi bir kümelenme (merkezli gauss, ikinci halka)
    eski profilin gizlediği kümelenme artefaktını geri getirirdi.
  - Elenen: (a) **merkezli gauss** — yine merkez yoğunluğu; "seyrek" rejimi
    sadece kuyruklarda; kapsam hesabı 2B integral; (b) **evsiz rastgele
    uniform hareket** — 10 u/sn × 30 sn = 300 birim difüzyon → geometri
    bulanıklaşır, koşul durağan değildir; (c) **jittersız ızgara** — tüm
    entity'ler 20×20 noktasında: komşuların payload'u byte-identik olur,
    `overlap_x` artefaktı.

- [x] **C1 — D1 break-even, ayrı prosesle ölçüldü** — spatial c5, ring
  profili (D1 sürekliliği), 30 sn, stagger 0,5 ms, pin (sunucu 8 fiziksel
  çekirdek). `server_cpu_s`/`clients_cpu_s` = 30 sn'de tüketilen çekirdek
  saniyesi (sunucu havuzu 16 mantıksal × 30 = 480 çekirdek-sn):

  | N | adım p50 | adım max | bütçe aşımı | server_hz | drop | late max | server_cpu_s (%havuz) | clients_cpu_s | out/conn |
  |---|----------|----------|-------------|-----------|------|----------|----------------------|---------------|----------|
  | 5 000 | 12,5 ms | 42,9 ms | %1,0 | 30,00 | 9 086 | 16 ms | 89,7 (%19) | 279,0 | 490,8 kbps |
  | 8 000 | 25 ms | 92,3 ms | %36,6 | 29,88 | 45 596 | 1,80 s | 120,8 (%25) | 319,7 | 359,3 kbps |
  | 9 000 | 25 ms | 103,6 ms | %44,6 | 28,19 | 30 869 | 2,34 s | 120,3 (%25) | 323,5 | 322,0 kbps |
  | 10 000 | 50 ms | 100,4 ms | %54,8 | 23,21 | 52 771 | 2,17 s | 119,4 (%25) | 298,8 | 269,6 kbps |

  **Cevap:** p50, 33,3 ms bütçeyi **9k ile 10k arasında** aşıyor (9k'da
  25 ms = bütçenin %75'i, hâlâ altında; 10k'da ≥50 ms = %150+, adımların
  %54,8'i bütçeli üstte, `server_hz` 23,2 < 30 → oda actor'ü ticker'dan
  geride, 252 late tick). **Sunucu CPU'su izole edildi ve doymadı:**
  ayrık çekirdek kümleri (`affinity=`), istemci decode'ı ayrı havuzda
  (`clients_cpu_s` ~280-320), sunucu havuzu 10k'da bile **%25** dolu
  (119/480 çekirdek-sn) — doyan parça **tek room actor'ünün serisel adım
  yolu** (read→convert→systems→encode→fan-out tek task'ta; 12 boş çekirdek
  varken adım 33,3 ms'e sığmıyor). Mimari çıkarım: bir sonraki kaldıraç
  oda paralelliği/segmentasyonu (P2), ek çekirdek değil. D1'in in-proc
  10k sayısı (p50 12,5 ms, %18,9) ile karşılaştırma: in-proc, p50 duvarını
  *maskeleyip* kuyruğu istemci decode gürültüsüyle bozuyordu (bimodal +
  1,34 s late) — duvar aynı odadaydı, ölçüm istemci CPU'su ile karışıktı.
  (Makine koşu sırasında paylaşımlı: loadavg ~10/32; pin, sunucu kümesini
  istemcilerden, ama arka plan kullanıcılarından korumaz — iki koşul grubu
  aynı makine hâlinde ölçüldü, karşılaştırma koşullar arası değil koşu
  içi.)

- [x] **C2 — Takım sisli uçtan uca (N=1 000, 20 sn, pin)** — aynı kod,
  üç geometri:

  | koşu | profil | out/conn (bps) | rec/tick | overlap_x | adım p50 | paket içeriği |
  |------|--------|----------------|----------|-----------|----------|----------------|
  | `all` | ring | 246 991 | 1 000,0 | 1,00 | 782 µs | 1 000/1 000 |
  | `team` | ring | 246 885 | 2 000,0 | 2,00 | 1 563 µs | 500 kendi + 500 düşman (kümüli: hiçbiri gizlenmez) |
  | `team` | **spread** | **177 526** | 1 153,2 | 1,15 | 1 563 µs | 500 kendi + ~77 düşman |

  `all` sayısı D2 sürekliliğini doğruluyor (D2: 247 813; Δ%0,3). Kümüli
  geometride `team` yine **sıfır kazanç** (D2 yeniden üretimi: 246 885 ≈
  246 991) — beklendiği gibi, strateji zayıflığı değil geometri
  özelliği. **Geniş haritada iddia doğrulandı:** "gerçek MOBA arenasında
  (geniş harita, seyrek görevli) aynı kod düşmanların çoğunu gizler" →
  ölçülen: takım paketinde 500 kendi + **~77 düşman** (1 153,2/2 − 500) →
  **düşman takımın ~%84,6'sı her an pakette yok**. Kazanç, kayıt sayısı
  (gerçek gizlilik etkisi) olarak **%42,3** (2 000→1 153 rec/tick;
  `all`'in %57,7'si görünüyor); bayt olarak **%28,1** (177,5 vs 247,0
  kbps) — fark, geniş haritanın kendisinden: |koordinat|≤1 000 → sint32
  zigzag 2 bayt (ring'de 1) → kayıt başına ~%25 daha büyük frame; gizlilik
  oranı kayıt sayısında, bant oranı koordinat kodlamasıyla seyreltiliyor.
  Not: 1 000 oyuncuda paket (≈7 KB) 1 400 B MTU'yu yine aşıyor
  (`snap_overflows` 1 186) — gerçek MOBA oyuncu sayısı (10-30) için
  aşılmaz; MTU sorunu ölçek sorunu, strateji sorunu değil.

- [x] **D — Takım, world state olarak (ve "grup = konum?" sorusunun
  yazılı cevabı)** — spec'in doğruluğu: önceki tasarımda takım `conn.id`
  paritesinden türetiliyordu, `group_of` **world okumuyordu** — gruplamanın
  mekansal olmadığına en kolay kanıttı (ve kolaylaştırdığımız buydu). Bu
  tur: takım üyeliği **dünya içi oyun durumu** oldu.
  - `TeamMember(Team)` componenti (join'de `on_join` yazar; kural — conn
    paritesi — `team_of`'da, artık "join anı kuralı" olarak dokümante);
    `group_of` conn → entity → **world'deki `TeamMember`** okur; AOI'nin
    `Position` okumasıyla birebir aynı şekil. `rebuild` sorgusu
    `(&WireId, &Position, Option<&TeamMember>)` (eski `ent_team` HashMap'i
    kaldırıldı).
  - **Runtime takım değişimi** testte kilitli: entity'nin `TeamMember`'i
    yazıldığında bir sonraki tick'te group takibe alır — entity 0. takımın
    paketinden düşer, 1. takımın paketinde **aynı wire id** ile belirir
    (pozisyonlar öyle seçildi ki iki takımın paketi de gözle görülür
    değişir; snapshots kayıt taşır, rol taşımaz → byte-identik paket
    ince ayrıntısı test dokümanında). **Kimlik değişmezliği korundu**:
    group geçişi ≠ wire kimlik geçişi.
  - **Aradık, yok:** Tasarımda gizli "grup = konum" varsayımı **kalmadı**.
    Dört stratejinin tümü aynı biçimde: "grup = f(world, conn)". Konumdan
    gelen anahtar (AOI/PVS) ile oyun durumundan gelen anahtar (takım)
    core'un grup mekaniklerinde (tick başına yeniden değerlendirme,
    grup başına snapshot/ledger, grup geçişinde kimlik sürekliliği)
    **birebir aynı yoldan** geçer; core'un ikisini ayırt eden tek bir
    dalı/özel durumu yoktur. Takım, "dünyada yaşayan oyun durumu"dur —
    mekansal değil, ama world dışında da değil.
  - **`VisionSource` eklenmedi (gerekçe):** Bugün onu `TeamMember`'den
    ayırt eden *okuyucu yok* — tüm görüş kaynakları, eşit config
    yarıçapıyla, tam da `TeamMember` taşıyan entity'ler. Eklemek,
    okuyucusuz ölü state olurdu (denetim turunda `Owner`'u kaldıran
    disiplinin aynısı). Seam hazır: ward/trap özelliği geldiğinde
    `rebuild`'in kaynak iterasyonu tek değişim noktası; sözleşme
    ("kaynak menzilinde görünür") değişmez.

Ham çıktı: `.scratch/*.txt` (gitignore; commit mesajında RESULT satırları).
Makine: AMD Ryzen 9 7950X 16C/32T, 124 GB RAM, rustc 1.95.0, taskset
2.42.2, paylaşımlı masaüstü (koşu sırasında loadavg ~10/32 — C1 koşuları
pin ile, C2 koşuları 8 fiziksel çekirdek sunucu havuzunda).


## Kapatılanlar (görünürlük stratejileri turu)

Bu tur "görünürlük"ü sunucu seçimi yapar: aynı oyun (aynı component'ler,
aynı hareket sistemi, aynı wire format) üzerinde **üç değiştirilebilir
görünürlük stratejisi** + taban (`all`), hepsi aynı `RoomLogic` seam'i
üzerinde; strateji `Config.visibility` ile seçilir (sunucu + `gsb-loadgen`).
Ayrıca: soyutlama kararı (C) ve D1–D3 yük ölçümleri. Test: 44 → **58**
(+14: 6 team + 6 pvs mantık testi + 2 actor-level; hiçbiri `#[ignore]` değil,
eski 44'ün hiçbiri değiştirilmedi).

- [x] **A1 — Mekansal/MMO (`AoiRoom`, `GroupKey=Cell`)** — bu turda
  davranışsal değişim yok; karşılaştırma tabanı olarak korundu (kod
  yalnızca C maddesiyle ortaklaştırıldı, bkz. aşağı).

- [x] **A2 — Takım sisli / MOBA (`TeamRoom`, `GroupKey=Team`, 2 grup)** —
  Bir takımın paketinde düşman entity, o takımın **görevli kaynaklarından
  en az birinin menzilindeyse** yer alır. Seçilen çözüm: takımlar `conn.id`
  paritesinden gelir (`team_of`); **grup anahtarı world okumaz** (konum
  bazlı gruplama `group_of`'u world'a bağlardı — group-snapshot'ın
  "grup = bağlantı sınıfı" sözleşmesiyle çelişir); menzil
  `team_vision_radius` konfigüredir (vars. 25.0). Görevli kaynak modeli:
  **her oyuncu entity'si eşit yarıçaplı görevli kaynağıdır**; sahibi olmayan
  (orphan/stamp'lı) entity **her iki takıma da** yayılır (yayınlanabilir
  küme = "Position taşıyor" değişmezini bozmaz). Aynı tick'te **tek kodlama
  / takım**: 2 snapshot, `Arc` ref'le paylaşım — 2 takımın paketi
  aynı-tick üretimi olduğundan istemci taraflı asimetri testi anlamlıdır.
  Kapanan hata sınıfı: "görünürlük client'a emanet" — hile testi
  (`tests/team.rs`): B takımı üyesi (10,0)'deyken A takımı üyesinin (40,0)
  snapshot'ı **hiç** düşman wire-id'ini taşımazken, B'nin snapshot'ı
  taşır (aynı tick, aynı dünya); entity görevliye girince **aynı wire id**
  ile var olur, çıkınca snapshot'tan düşer (istemci "kayboldu" okur —
  self-contained sözleşme; delta/öncül-olay yok). Elenen: (a) **ward/sahipli
  görevli componenti** — spec'teki "unit ve/veya ward" bu turun "aynı
  component seti" çerçevesinde yeni component gerektirir; `VisionSource {
  radius, owner_team? }` gibi bir component stratejiyi ifade edebilirdi ama
  tur çerçevesi (yeni oyun mantığı eklememek) bunu dışarıda tuttu — eşit
  yarıçaplı oyuncu-kaynak modeli aynı testleri geçirir, ward desteği
  component eklemekle geri getirilebilir (sözleşme değişmez: "kaynak
  menzilinde"); (b) **bağlantı başına grup** (`GroupKey=ConnectionId`) —
  her istemcinin menzili farklı olsa bile takım sisli menzil
  **takım-bazlı** olduğundan bağlantı başına grup hem 2 grubu bozar hem
  grup başına paylaşılan baytı (asıl kazanç) sıfırlar; (c) **konum bazlı
  takım** — takım bir oyun durumudur, konumdan türetilmez (harita yarısı
  takımla takım sisli olmaz).

- [x] **A3 — PVS/FPS (`SectorRoom`, `GroupKey=Sector`)** — Harita elle
  yazılmış **4 kavisel (convex) sektör** + **statik görünürlük tablosu**
  (`VISIBLE_FROM[S]`: bit mask'ı; A↔B arası duvar, kuzey bandı C/D ile
  açık); `sector_of` = kavisel test (cross product işaretleri, CCW
  çokgen); harita dışı konum `OUT` catch-all sektörü (yayınlanabilir küme
  değişmezini harita sınırı bozamaz). Grup başına snapshot = görünürlük
  tablosundaki sektörlerin bucket birleşimi; hücre geçişi `group_of`
  yeniden değerlendirmesiyle otomatik, **wire id değişmez** (test kilitli:
  `tests/pvs.rs`). Kapanan hata sınıfı: "mesafe testi görünürlüğü
  kanıtlamaz" — 3 birim aradaki iki entity **bağlantısız** sektörlerde
  birbirini GÖRMEZ (A(-1,0) ile B(2,0): 3 birim, duvar → ayrık snapshot'lar);
  bağlantılı sektörler (A↔C) görür; sektör geçişinde aynı id taşınır.
  Elenen: (a) **BSP/oklüzyon derleyicisi** — spec açık: "BSP derleyicisi
  yok"; elle convex sektör + tablo bu arenada (100×100, 4 bölge) derleyici
  maliyetini gerektirmez, tablo statik olduğu için tick'te sıfır maliyet;
  (b) **tick başına ray-cast / mesafe testi** — O(entity²) ışın atımı ve
  "3 birim mesafe = görünür" yanılgısını (hata sınıfının kendisi) üretir;
  duvar/koşu koridoru ayrımı mesafede kodlanamaz; (c) **sektör = AABB
  kesişimi** — kavisel olmayan bölgede "görünürlük ⊆ mesafe" garantisi
  bozulur (AABB köşeleri birbirini görmeyen bölgeyi görünürlük alanına
  sokar); convex + tablo, "tablodaki sektör kümesi = kesin görünürlük
  alanı"ı elle doğrulanabilir yapar.

- [x] **B — Strateji seçimi (sunucu config + loadgen) + varsayılan** —
  `Config.visibility: all | spatial | team | pvs` (serde lowercase;
  `config.example.toml` belgeli); `gsb-loadgen --visibility …` aynı seçimi
  yapıyor (`--cell-size`, `--vision-radius` strateji parametreleri).
  **Varsayılan: `all`.** Gerekçe (üçlü): (1) **geri uyumluluk** — önceki
  tüm yük ölçümleri `all` (eski `aoi=false` taban) üzerinden alındı;
  varsayılan değiştirse eski sayılarla karşılaştırma kırılır; (2) **ölçüm
  tabanı** — `all` en kötü durum (en çok kodlama + bant) olduğundan her
  stratejinin kazancı/bedeli onun karşısında net okunur; (3) **en az
  sürpriz** — görünürlük kısıtlama bir **oyun kararıdır** (takım sisli
  istemeyen bir MMO, PVS istemeyen bir MOBA); sessizce kısıtlanmış
  görünürlük bug olarak algılanır, açık seçim ise özellik. Elenen:
  `spatial` varsayılanı (en yaygın MMO profili argümanı — ama 9× kodlama
  maliyetini varsayılan yapar ve önceki taban verisiyle kesintisiz
  karşılaştırmayı bozar; hücre boyutu yanlış seçilirse net kayıp — ölçülen,
  bkz. D2).

- [x] **C — Soyutlama kararı: `Visibility` trait'i GEREK YOK; `RoomLogic`
  yeterli seam. Ortak mekanik `gsb_game::common`'e taşındı.** Karar,
  üç strateji yazıldıktan sonra koddan:
  - **Trait gerekmez** çünkü stratejiler arasındaki gerçek fark,
    `RoomLogic`'in zaten ayırdığı iki şeydir — `GroupKey` tipi +
    `group_of`/`snapshot` içeriği. Bir `Visibility` trait'i aynı farkı
    yeniden soyutlar; üstelik stratejilerin içerik hesapları
    (küme birleşimi / mesafe önelemi / statik tablo) ortak bir imzaya
    sığacak kadar da benzer değildir — trait'in arkasında duracak ortak
    bir uygulama yoktur. Kanıt: bu turda 2 yeni strateji + 1 yeni metrik
    metodu eklendi, `gsb-core`'ün trait **şekli değişmedi** (tek core
    değişimi ekleyici: `RoomLogic::encoded_records()` default metodu +
    `snap_records` sayacı — D3 ölçümü için; core'un test logic'leri
    default 0 ile davranışsal değişim yok).
  - **Ortak olan gerçek şey** strateji değil, oda **muhasebesi**: runner
    kurulumu, `conn_entity` tablosu, wire sayaç + minting, `on_join` /
    `on_leave` / `ingest` (MOVE_TO decode + guard'lar), sistem yürütme,
    orphan stamp'leme — 4 odada **birebir aynı ~45 satır** (4 kopya).
    Bunlar `gsb_game::common` modülüne tek kopya olarak taşındı (traitsiz
    düz fonksiyonlar; odalar kendi alanlarını korur — inline testler
    `room.conn_entity` vb.'ye erişiyor, 44 eski test satırı bile
    değişmedi). `common::next_serial` artık `WireId::new`'ün **tek
    çağrıcısı** (mint noktası tek yerde; `components.rs` + `room.rs`
    doküman referansları güncellendi). Bir 5. stratejinin maliyeti ~250 →
    ~80 satıra düştü (sadece `GroupKey`, `group_of`, `snapshot` + içerik
    hesabı).
  - Elenen: (a) `Visibility` trait'i (yukarıda); (b) `RoomBook` struct'ı
    (alanları sahiplenen ortak struct) — odaların alan adlarını değiştirir,
    44 eski testin içindeki `room.conn_entity` erişimini kırardı; test
    silme/değiştirme yasak olduğundan fonksiyon-over-alan deseni seçildi;
    (c) "hiçbir şey yapma" — 4 birebir kopya gerçek kod; "spekülatif değil"
    kriteri bu duruma tam uyar (dört kopya mevcut, beşincisi gelebilir).

- [x] **D1 — Mekansal AOI break-even (ölçüm, tahmin değil)** — 30 Hz,
  release, in-proc (istemciler sunucuyla CPU paylaşır → muhafazakâr
  alt sınır), hücre 5, stagger 1 ms, 15 s; makine: AMD Ryzen 9 7950X
  16C/32T, 124 GB RAM, rustc 1.95.0; masaüstü paylaşımlı (arka plan
  ~2-3 çekirdek: argos/thunderbird vb. — 1 dk loadavg ~9-11/32).

  | N | adım p50 (µs) | adım max (µs) | bütçe aşımı | drop | late tick max | tepe payload (B) | aşım (1400 B) | grup | out/conn (bps) |
  |---|--------------|---------------|-------------|------|---------------|------------------|---------------|------|----------------|
  | 2 000 | 6 250 | 21 737 | %0 | 0 | 42 µs | 11 372 | 12 895 | 26 | 80 991 |
  | 4 000 | 12 500 | 27 040 | %0 | 0 | 2 010 µs | 19 645 | 25 127 | 35 | 127 464 |
  | 6 000 | 25 000 | 55 565 | %3,8 | 0 | 42 290 µs | 25 136 | 32 033 | 48 | 146 978 |
  | 8 000 | 25 000 | 60 305 | %12,2 | 3 869 | 271 745 µs | 26 892 | 37 090 | 76 | 148 088 |
  | 10 000 | 12 500 (bimodal) | 86 862 | %18,9 | 54 263 | 1 343 687 µs | 23 570 | 39 370 | –* | 108 267 |

  *n=10 000 son raporu istemcilerin ayrılma (drain) sırasında örneklenmiş
  (groups=1, members=1); tüm istemciler join etmiş (connected=joined=10 000).

  **Cevap (ölçülen, tahmin yok):** p50 adım süresi **N≤10 000'de 33,3 ms
  bütçeye ulaşmıyor** (10 000'de p50 12,5 ms — bimodal: çoğu adım hızlı,
  kuyruk yavaş). Önceki turun ~10-15k **tahmini** böylece "bütçe p50'de
  10k'ın üzerinde" olarak daralıyor; ama p50 tek doğru gösterge değil:
  **bütçe aşan adım oranı 6k'da %3,8 → 10k'da %18,9**, max 86,9 ms (bütçenin
  2,6×), ve 10 000'de **54 263 batch drop + 1,34 s late tick** — oda
  actor'ünün ticker'dan geride kalması (catch-up adım patması; ticker'ın
  kendisi burst yapmaz — `ticker.rs` resync eder, ölçülen hz=42,5
  room-yana catch-up'un izi). 2k-8k arası 1 dk loadavg ~9-11/32 olduğuna
  göre makine bütünü doymuş değil; doyan parça **in-proc istemci decode
  yolu** (10 000 istemci × ~20 KB/snapshot × 30 Hz ≈ 6 GB/s decode, 32
  çekirdeğe yayılmış) — fan-out kanalları dolar, oda drop sayar. Yani
  ölçülen duvar "oda adımları bütçeyi aşıyor" değil: **p50 bütçesi 10k'ya
  kadar korunan, kuyruk (p99/max) 6k+ da aşan, ve 10k'da in-proc istemci
  katmanının doyuğuyla karışan** bir rejim. Temiz oda-CPU duvarını
  ayırmak için ayrı prosesli istemci gerekir (P2 — bu turda yapılmadı;
  in-proc sayısı muhafazakâr alt sınırdır: istemci CPU'su sunucu adımıyla
  yarışıyor). (loadavg monitörünün 40×3 s penceresi 10 000 koşusunun
  başında bitti — 10 000 için 1 dk loadavg örneği yok; yukarıdaki 9-11
  değeri 2k-8k penceresine aittir.)

- [x] **D2 — Üç strateji, aynı yük (N=1 000, 15 s, stagger 1 ms,
  move 150 ms, MTU 1400 B)** — aynı `gsb-loadgen`, aynı hareket profili,
  aynı makine (profil: yarıçap-40 **hedef** halkası, 4 rad/sn → hedef 160
  u/sn; entity 10 u/sn → entity'ler hedefin **peşinde merkez bandında**
  kümelenir; ölçülen ayak izi: PVS'te sadece A+B sektörleri dolu, c5'te
  ~16 hücre, max hücre 118 üye). Bu geometri yorum için kritik:

  | strateji | out/conn (bps) | sunucu out | adım p50 (µs) | max (µs) | bütçe aşımı | tepe payload (B) | aşım | rec/tick | overlap_x |
  |----------|----------------|-----------|---------------|----------|-------------|------------------|------|----------|-----------|
  | `all` | 247 813 | 247,8 Mbit/s | 3 126 | 8 165 | %0 | 8 809 | 445 | 1 000,0 | 1,00 |
  | `spatial` c20 | 180 493 | 180,5 Mbit/s | 3 126 | 7 962 | %0 | 8 678 | 4 521 | 5 496,0 | 5,50 |
  | `spatial` c5 | 44 845 | 44,8 Mbit/s | 3 126 | 9 392 | %0 | 5 993 | 5 118 | 8 282,3 | 8,28 |
  | `team` | 247 825 | 247,8 Mbit/s | 1 563 | 4 368 | %0 | 8 819 | 890 | 2 000,0 | 2,00 |
  | `pvs` | 126 002 | 126,0 Mbit/s | 1 563 | 4 481 | %0 | 5 694 | 1 333 | 1 367,5 | 1,37 |

  **Yorum (ölçüm, varsayım değil):**
  1. **Bant (stratejinin asli amacı):** c5 `all`'e göre **%82** az
     (247,8→44,8 kbps/conn) — önceki turun c5 @1000 sayısıyla (39,7 M)
     tutarlı; PVS **%49** az (126,0 kbps/conn); c20 %27 az.
  2. **Team, bu geometride sıfır bant kazancı:** 25 birimlik menzil +
     merkez bandı kümelenmesi → her iki takım da haritanın tümünü görüyor
     (rec/tick=2 000 = tam dünya × 2; out/conn `all` ile birebir aynı
     247,8 kbps). Bu bir strateji zayıflığı değil, **yük profili özelliği**:
     gerçek MOBA arenasında (geniş harita, seyrek görevli) aynı kod
     düşmanların çoğunu gizler; burada 1 000 entity'nin ~%50'si her
     takımdan her görevli menzilinde. Ölçüm dürüstçe bunu gösterir: team
     sisli, yoğun merkezî kümelenmede bedelini (2× kodlama) öder,
     kazancı sıfırdır.
  3. **PVS, bu geometride %49 bant + 1,37× kodlama:** kümelenme güney
     bandında (A/B) olduğundan her oyuncu dünyanın ~yarısını görüyor
     (A→A∪C, B→B∪D); kuzey sektörleri (C/D) boş. Kuzeyi dolduran bir yük
     PVS kazancını değiştirir (tablo statik; davranış kodda değil ölçümde
     değişir).
  4. **Adım süresi:** 1 000'de beş strateji de bütçenin %5'inde (p50
     1,5-3 ms; histogram kuantumlu — 1 563/3 126 µs iki komşu kutu);
     strateji seçimi bu ölçekte CPU'da görünmez, bantta görünür. CPU
     farkı D1'de (N ölçeğinde) beliriyor.

- [x] **D3 — Overlap ölçümü + "birim başına tek kodlama" kararı:
  UYGULANMADI (ölçüm + eşik ile gerekçeli)** — `overlap_x` =
  tick başına kodlanan kayıt / üye sayısı (`RoomLogic::encoded_records`
  ile ölçüldü, bkz. C maddesi): `all` 1,00 · PVS 1,37 · team 2,00 ·
  c20 5,50 · c5 8,28. Önerilen tasarım: atomik parça (hücre/sektör) başına
  **bir kez** kodla, istemci gerekli parçaların frame'lerine
  "abone" olsun (batch içi k küçük frame; boş parça da paketsiz kalamaz —
  self-contained union'un eksiksizliği için). Takas: kodlama
  overlap_x → 1,0'a iner; karşılığında istemci tick başına **k frame**
  taşır (spatial c5: k=9 hücre; PVS: k=2-3; team: k≈N/2 — pratik
  sonsuz). **Eşik:** tasarımı uygulamak, kodlama tasarrufunun istemci
  başına k ek frame'in maliyetini aştığı durumda anlamlı:
  `overlap_x − 1 ≥ k · (c_frame/c_enc)`. Ölçülenler: (a) N=1 000'de
  kodlama hacminin 1 000→8 282 kayıt/tick (8,3×) aralığındaki tüm farkın
  adım ortalamasına etkisi **~%11** (1 891→2 104 µs) → bu ölçekte
  marjinal kodlanan kayda düşen pay ≪ 1 frame fan-out payı (fan-out,
  D1'de 8-10k'ta drop üretici olan yol); (b) D1: doyan eksen **istemci
  başına iş** (drop + late tick), kodlama µs'leri değil. Eşik sayılarla:
  c5 için `overlap_x ≥ 1 + 9·(c_frame/c_enc)`; ölçüm
  `c_frame ≥ c_enc`'e uyumlu (fan-out yolu doyan yol) → eşik ≈ 10;
  ölçülen 8,28 **eşiğin altında**. PVS (k=2-3): eşik ≈ 3-4, ölçülen 1,37 —
  çok altında. Team (k≈N/2): eşik ≈ 500, ölçülen 2,0 — asla. **Karar:
  uygulanmadı.** Mevcut mimari zaten "grup başına bir kez kodla, `Arc`
  ref ile paylaş" — kalan overlap, görünürlük öneliminin doğrudan
  sonucudur (bir entity birden çok gruba görünür); onu sıfırlamak parça
  tanımıni atomiğe (hücre) indirir ve bedelini istemci başına frame sayısına
  yazar. Ölçek büyüdükçe (D1: 10k) bu takas daha da kötüleşir: kodlama
  tasarrufu ∝N iken ek frame maliyeti ∝k·N. Tekrar değerlendirme koşulu
  (eşik): `overlap_x` ölçümü 10'u (spatial k=9, c_frame≈c_enc) aşarsa —
  örn. 3×3 yerine 5×5 blok, ya da 9×'ten çok komşuluk taşıyan bir görünürlük
  kuralı seçilirse.

- **Bu turda bilinçli olarak yapılmayanlar:** (1) Ayrı prosesli
  istemcilerle D1 (oda-CPU duvarını in-proc decode'dan ayırmak — P2'ye
  kalır; in-proc sayılar muhafazakâr alt sınırdır ve bu turda yeterli
  soruyu — "p50 bütçe 10k'a kadar dayanır mı, kuyruk nerede aşılır" —
  yanıtladı); (2) loadgen hareket profilinin "entity'ler hedef halkasında"
  olması (hedef 160 u/sn, entity 10 u/sn → merkez bandı kümelenmesi;
  önceki turun sayılarıyla süreklilik için profil değiştirilmedi —
  yorumlaması her tabloda belirtildi); (3) PVS haritasının
  parametrize edilmesi (elle 4 sektör bu turun arenası; gerçek harita
  yazarlığı oyunun malı); (4) team stratejisinde ward/`VisionSource`
  componenti (A2 maddesi, spec gerginliği — "aynı component seti"
  çerçevesi eşit-yarıçaplı oyuncu kaynaklarıyla sınırlandı); (5)
  `encoded_records`'ün delta yayınına (P2) genişletilmesi.

- **Spec gerginliği (rapor):** A2'de "görevli kaynak = unit **ve/veya
  ward**" ifadesi, turun "aynı component seti" çerçevesiyle çelişir —
  ward, `VisionSource` gibi **yeni** bir component ister. Tur çerçevesine
  sadık kalındı: her oyuncu entity'si eşit yarıçaplı kaynak; orphan
  entity'ler her takıma yayılır. Ward desteği, sözleşmeyi (kaynak
  menzilinde görünür) bozmaksızın tek component eklemeyle geri
  getirilebilir.

Ham çıktı: `target/loadout/*.txt` (gitignore; commit mesajında RESULT
satırları). Makine: AMD Ryzen 9 7950X 16C/32T, 124 GB RAM, rustc 1.95.0,
paylaşımlı masaüstü (arka plan ~2-3 çekirdek).


## Kapatılanlar (metrik düzeltme + AOI turu)

Bu tur iki işi kapatır: (A) metrik altyapısının üç ölçüm hatası (spec),
(B) AOI — tek odada bağlantı başına bant genişliğini O(entity) → O(görünürlük
kümesi) yapar; ana soru "**`gsb-game` içinde `gsb-core`'e dokunmadan yapılabilir
mi?**" → **evet** (aşağıda).

- [x] **A1 — Adım histogramı tick bütçesine göre (oran)** — Eski
  `HIST_EDGES_US=[50,100,250,500,1000,5000]` (mutlak µs) üst kutusu
  `[5000,∞)`'dü; 30 Hz bütçesi 33.3 ms olduğundan 5.1 ms'lik ve 40 ms'lik
  adım **aynı** kutuya düşüyordu ve "bütçe aşıldı mı?" histogramdan
  okunamıyordu. Yeni: kenarlar tick bütçesinin (bir periyot, µs) **oranları**
  — log-2 merdiveni `1/128× … 1× … 32×` (`HIST_EDGES`). `(1,1)` kenarı **tam
  tick bütçesidir**; bin `HIST_OVERFLOW_BIN` (=8) ve üzeri **bütçe aşımı**
  (oda hızını tutamıyorsa). Aşağıya 1/128×'e inen basamaklar sağlıklı oda
  (adım ≪ bütçe) için: 30 Hz'de (bütçe 33 333 µs) alt kutular
  ~261/521/1042 µs'e oturur — gerçek adım sürelerinin (≈500 µs) yeri; p50
  artık anlamlı (eski hâl her şeyi tek devasa `[0, 0.5×bütçe)` kutusuna
  yığıp p50≈8.3 ms sahte değeri veriyordu — ölçüldü, düzeltme sonrası
  500'de p50≈391 µs). Neden oran (mutlak µs değil): mimari 15/30/60 Hz oda
  destekliyor (DESIGN §10); oran bütçeyi **her hızda** bir kutu sınırı yapar —
  spec'in kriteri tam "bütçe aşımı histogramdan okunabilir mi". Kanıt:
  `hist_index_binning` (30 Hz bütçede 5.1 ms ≠ 40 ms; 40 ms'ı
  `HIST_OVERFLOW_BIN`). Kapanan hata sınıfı: "üst kutu bütçeyi göremiyor".
  Elenen: (a) mutlak µs + bütçeyi ayrı tutmak — kriteri karşılar ama iki
  kaynak (kenar+bütçe) her hızda ayrı ayar ister; (b) 1/2×'ten başlayan kısa
  merdiven — kriteri karşılar ama sağlıklı odanın dağılımı tek kutuya yığılır
  (p50 bozulur, ölçülen).

- [x] **A2 — Örnek gönderim temposu rapor temposuna bağlandı + oran örnek
  aralığı üzerinden** — Oda actor'ü eskiden **her tick'te** (30/sn) tam
  `RoomSample` gönderiyor; toplayıcı yalnız `acc.latest`'i tutup 1/s rapor
  üretiyordu → 29/30 örnek atılıyordu. `RoomConfig.metrics_cadence_hz`
  (vars. 1.0) ile oda her `round(tick_hz/cadence)` adımda (vars. 30) bir kez
  gönderiyor; sayaçlar kümülatif olduğu için bu birleştirilebilir. **İkinci,
  ölçümle bulunan ince hata:** 1 Hz örnek + 1 Hz rapor **faz-kilitli değil**
  → bir rapor penceresi 0–2 örnek kapsayabilir; oranı rapor penceresi
  üzerinden (Δadım/rapor-Δt) almak sahte hız verir (loadgen `server_hz`
  58/198 Hz — tutarlı yeniden üretildi). Düzeltme: her örnek kendi
  `emit_at`'ını taşıyor (oda `Instant::now()`); oran **örnek aralığı**
  üzerinden (`latest.emit_at − prev.emit_at`), rapor penceresi üzerinden
  değil. Kanıt: `accumulator_applies_events_and_computes_rates` (emit_at 1 sn
  aralık → hz=30) + `loadgen_smoke` (server_hz 30.00, 3/3 stabil). Kapanan hata
  sınıfı: "gönderim temposu ≠ rapor temposu → atılan örnek" + "oran penceresi
  ≠ örnek aralığı → sahte hız". Elenen: (a) rapor penceresi oranı — faz
  kaymasında yanlış (ölçülen); (b) toplayıcının varış anı — varış≈emit ama test
  edilemez (test `Instant`'i kontrol edemez) ve iletme gecikmesi oranı kirletir;
  (c) örnekleri kanalda biriktirmek — örnek başına daha çok bellek; `emit_at`
  tek alanla yeterli.

- [x] **A3 — Metrik kanalı bounded + `try_send` (DESIGN §2 uyumu)** — Eskiden
  `mpsc::unbounded` ("geri-baskı fiilen sorun değil" diye, DESIGN §12); ama §2
  disiplini (paylaşımsız, sınırlı kaynak) ve gerçek bir toplayıcı sarkarsa
  unbounded kanalın bellek sızması riski. Yeni: `mpsc::bounded(4096)` +
  **senkron `try_send`** (Full → üretici `metrics_dropped` sayacını artırır,
  örnek atılır). `try_send` `send` gibi **senkron** (Future değil) → oda
  tick'ine await **eklenmez** (spec'in sert şartı korunur; bounded `send`
  Future olduğu için `send` değil `try_send` — `OutSink::flush` +
  `dropped_frames` ile aynı desen). Atılma zararsız (sayaçlar kümülatif; bir
  örnek kaybı oranı bir pencere bozar, birikimi değil). `metrics_dropped`
  rapor satırına eklendi (oda/registry/conn başına delta). Kapasite 4096
  (üç üretici × oda başına 1 örnek/sn × marj) — `start_inner`'de sabit.
  Kapanan hata sınıfı: "unbounded kanal sınırlı-kaynak disiplinini ihlal
  edebilir". Elenen: (a) unbounded (eski) — toplayıcı sarkarsa bellek sızması;
  (b) bounded + `send` (await'li) — oda tick'ine await ekler, spec ihlali;
  (c) `tokio::select!` (kanal+tick) — gsb-lint yasak; (d) toplayıcıda
  paylaşımlı ring-buffer — §2'yi bozar.

- [x] **A-ek — `snap_overflows` sayacı** — `max_snapshot_bytes` (vars. 1400)
  aşan snapshot sayısı, broadcast fazında sayılıyor (grup başına kodlanan
  paketin boyutu; grup bazlı, bağlantı bazlı değil). AOI'nin MTU sinyali:
  tüm-dünya snapshot'ı ölçekte her tick aşar, hücre başına snapshot aşmaz
  (aşağıdaki ölçüm doğruluyor). Rapor + loadgen RESULT satırına eklendi.

- [x] **B — AOI (görünürlük): `gsb-game` içinde, `gsb-core`'e dokunmadan** —
  **Ana sorunun cevabı: EVET.** Neden soyutlama sızması yok: çekirdeğin grup
  mekanizması AOI'nin gerektirdiğinin **tamamını** zaten sağlıyor —
  `RoomLogic::group_of(world, conn)` her tick yeniden değerlendiriliyor (hücre
  geçişi otomatik, yeni hücrenin bloğu), grup başına snapshot **bir kez**
  kodlanıp üyeyle `Arc` ref'iyle paylaşılıyor (§4), "değişiklik yoksa yayın
  durur" defter eşitliği **üyelik+konumu** içeriyor. AOI yalnızca mekansal bir
  `GroupKey` (`Cell(i32,i32)`, `gsb_game::aoi`) + hücre başına 3×3 komşuluk
  defteri sunuyor; `gsb-core` değişimi gerekmedi (bu turdaki gsb-core farkı
  %100 A maddeleridir — `git diff` ile doğrulandı). Alternatif (çekirdeğe
  `Visibility` trait'i / hücre kavramı eklemek) gereksiz soyutlama olurdu.
  **Görünürlük kümesi** = oyuncunun hücresinin merkezli **3×3 blok**
  (`RADIUS=1`; hücreler `floor(pos/cell_size)`). Oyuncu başına yarımçap
  REDDEDİLDİ: farklı küme başına → `GroupKey`'i `ConnectionId` yapmak gerekir →
  grup başına kodlama → bant kazancı sıfır + çekirdek değişikliği. **Hücre
  boyutu konfigüredir** (`Config.aoi_cell_size`, vars. 20.0) — sabit değil,
  çünkü doğru boyut yoğunluk/arena/MTU'ya göre değişir (aşağıda). Kimlik
  değişmezi korunur: wire id `on_join`/yeni `Position` damgasında `next_serial`
  ile **bir kez** basılır, hücre değişiminde DEĞİŞMEZ; sonradan giren kontrol
  fazında dünyaya girdiği için kendi hücresinin ilk bloğunda **tam kümesini**
  görür. Test kilitli: `gsb_game::aoi::tests` (5 mantık) + `tests/aoi.rs`
  (actor: fan-out, hücre geçişi, kimlik).

  **Ölçülen ticaret** (30 Hz, MTU 1400, in-proc, 15 s; Ryzen 9 7950X 16C/32T,
  rustc 1.95.0; 2000'de 1 ms stagger):

  | N | AOI | hücre | out/conn (bps) | toplam out | adım ort. (µs) | adım max (µs) | tepe payload (B) | aşım | grup |
  |---|-----|-------|----------------|-----------|----------------|---------------|------------------|------|------|
  | 500 | off | – | 118 869 | 59.4 M | 533 | 1 183 | 4 347 | 423 | 1 |
  | 500 | on | 20 | 94 788 | 47.4 M | 802 | 1 941 | 4 281 | 3 209 | 4 |
  | 1000 | off | – | 208 643 | 208.6 M | 1 535 | 5 071 | 8 210 | 443 | 1 |
  | 1000 | on | 20 | 185 787 | 185.8 M | 1 967 | 4 389 | 8 668 | 4 626 | 4 |
  | 1000 | on | 12 | 86 251 | 86.3 M | 1 479 | 4 817 | 6 811 | 5 167 | – |
  | 1000 | on | 8 | 64 064 | 64.1 M | 1 794 | 5 327 | 7 001 | 5 534 | – |
  | 1000 | on | 5 | 39 675 | 39.7 M | 2 077 | 3 468 | 5 717 | 4 824 | – |
  | 1000 | on | 3 | 7 001 | 7.0 M | 1 377 | 7 595 | 1 689 | 188 | – |
  | 2000 | off | – | 476 335 | 952.7 M | 6 192 | 11 453 | 17 699 | 445 | 1 |
  | 2000 | on | 5 | 80 972 | 161.9 M | 4 256 | 8 895 | 11 308 | 12 918 | 28 |

  **Yorum (ölçüm, varsayım değil):**
  1. **Kazanç hücre boyutuyla, ölçekte değil (bu iş yükünde).** Yarıçap-40
     halka 100×100 arenayı ~80×80'a sığar; c20 bloğu (60×60) halikanın
     ~%80'ini kavrar → bant kazancı küçük (c20'de %11-20). Küçük hücre
     (c5) bloğu küçük bir yayı kaplar → **%81-83** kazanç (1000/2000'de).
     Yani "büyük G'de kazanç" burada **mutlak** kazanç olarak geçer: toplam
     bant ~N² büyür (476 kbps/conn @2000) ve AOI'nin mutlak tasarrufu o
     ölçekte 952→162 MB/s'e çıkar; oran (hücre başına) ölçekte ~sabit.
  2. **MTU (1400) hücre boyutunu belirler.** ~12 B/kayıt → 1400 B ≈ 116
     kayıt/blok; blok 3×3=9 hücre → **~13 kayıt/hücre** hedef. c3 @1000
     tepe 1 689 B (hâlâ %13 aşım, 188/450 tick); c2 ~1400'ün altına iner.
     c20+ ölçekte **her tick** aşar (8 668 B). Yani MTU uyumu için hücre,
     N büyüdükçe **küçülmeli** (yoğunluk ∝ N).
  3. **AOI CPU maliyeti ~9× kodlama** (her kayıt 9 komşu bloğa girer) ama bu
     ölçekte bütçeye değmiyor: adım ort. 500'de 533 µs, 2000'de 6 192 µs (off) / 4 256 µs (c5) — bütçenin (33 333 µs) %6-18'i, `over_budget=0%`. **Break-even
     (adım → bütçe) N=2000'in ÜSTÜNDE**; doğrusal dışa vurumla (6 ms@2000 →
     33 ms) ~10-15k üye. 2000'in altında AOI (iyi hücreyle) **net pozitif**:
     büyük bant kazancı + bütçe altı CPU.
  4. **Yeni darboğaz: adım süresi (CPU = kodlama + fan-out), bant değil.**
     N büyüdükçe adım süresi ~N büyür (in-proc'ta istemci decode'üyle
     süper-lineer); bant AOI ile düşürüldüğü için ilk doyaan adım bütçesi
     olacak (~10-15k). Kötü hücre (c20) ölçekte **net kayıp** olur: 9× kodlama
     karşılığında yalnız %11 bant — CPU maliyeti büyürken kazanç sabit kalır.
     "Küçük G'de net kayıp" bu ölçekte (bütçe gevşek) görülmüyor; bütçe
     sıkışsa (yüksek tick hızı / dolu oda) 9× kodlama küçük G'de bütçeyi
     aşabilir.


## Kapatılanlar (metrik + yük turu)

- [x] **`WireId` mint tarafı kapandı (tip düzeyinde)** — yayınlanabilirlik
  turunda "yapısal olarak kapatılamaz" denilen son parça: field private
  (`WireId(u64)` artık hiçbir crate'ten, crate içinde de `components`
  modülü dışından `WireId(42)` diye inşa edilemez) + `Default` derive'ı
  kaldırıldı (`WireId::default()` = 0 = "atanmamış" kimliği üretilemez).
  Tek inşaat yolu `WireId::new(u64)` (`pub(crate)`, `const`); okuma tarafı
  açık kalmalıydı (`pub const fn get(self)`) — kimlik okumak meşru,
  üretmek değil. Tek minting noktası `DemoRoom::next_serial()`
  (sayaç `next_wire_id`): iki çağrı noktası (`on_join` + broadcast
  yetim-damgası) bu tek noktanın üzerinden geçer. Kapanan hata sınıfı:
  "kimlik üretimi sözleşmede yaşıyordu" → artık üretimin tek yolu tipin
  kendi kapısı ve pratikte tek sayaç. Elenen alternatifler:
  (a) `pub fn new`'i herkese açık tutmak — her crate mint edebilir,
  çakışma alanı kapanmıyor (spec: tek nokta isteniyordu).
  (b) `Arc<AtomicU64>` paylaşımlı sayaç — paylaşımlı durum = DESIGN §2
  ihlali; zaten sayacın sahibi oda actor'ünün yerel durumuydu (tek sahip
  mevcut). (c) Sealed trait/free function seremonisi — aynı kapanımı
  daha fazla yüzeyle verirdi.

- [x] **Metrik altyapısı (P0 "Temel metrik")** — Sayaçlar **ait olduğu
  aktörün yerel durumunda** ve dışarı **kanalla** taşınır
  (`gsb_core::metrics`): oda actor'ü her adım sonunda (tick gövdesinin
  son satırı, senkron) `MetricsEvent::Room(RoomSample)` gönderir;
  registry olay başına (open/close/join/leave/room); bağlantı actor'ü
  ≤1/sn + kapanışta son gönderim (in/out byte/frame delta). Kanal
  `mpsc::unbounded` — **`send` senkron** (`Result` döner, `Future`
  değil): oda actor'ünün tick döngüsüne **await eklenmedi**; tek await
  hâlâ `tick_rx.recv()`'tir (spec'in sert şartı — bounded kanal
  `send`'i Future olduğundan elendi). Tüketiciler:
  `MetricsCollector` görevi — tek await'i ticker'ın **aynı**
  broadcast aboneliği (odalarla aynı saat kaynağı; tek-await
  disiplininin aynası); her tick'te `try_recv` ile boşaltır, rapor
  süresi (varsayılan 1 s) dolunca `MetricReport` üretir, ticker
  kapalıysa son raporu basıp temiz çıkar (shutdown kaskadının doğal
  parçası — DESIGN §9). Sink'ler: `Log` (`RUST_LOG=info` →
  `gsb-metric scope=.. key=value` satırları) ve `Channel`
  (→ `MetricReport` — `start_server_metrics` ve testler bu yoldan).
  Ölçülenler (spec'in istediği sorular): konfigure **vs gerçek** tick
  hızı (oda `hz` Δadım/rapor + registry oda sayısı), **adım süresi
  dağılımı** (min/mean/max + 7 kutulu histogram µs), **gecikme**
  (`late_*` — tick'e ulaşma gecikmesi) ve `lagged_*` (broadcast
  tamponu aşımı), **atılan batch** (`dropped`), **keepalive yeniden
  gönderimi** (`keepalive_resends`), **grup sayısı/üye**
  (`groups`/`members`/`max_group`), **snapshot yükü**
  (`snapshots`, `snap_bytes_s`, `snap_bytes_max`, `shipped_*` —
  kodlanan vs dağıtılan), **bağlantı sayısı/akışı**
  (registry `conns`/`opens`/`closes`/`joins`/`leaves`), **byte
  in/out** (conn actor delta + net toplam). Kapanan hata sınıfı:
  "log satırı dışında ölçüm yoktu" — her sunucu süreci artık
  kendisinden (log ya da kanal) okunabilir. Kanıt testi
  (`metrics::tests::room_counters_flow_to_collector`): gerçek
  `RoomActor` + gerçek collector — oda actor'ünün **yerel** sayacı
  (özellikle `dropped > 0`: tüketilmeyen çıkış kanalı) actor'ün
  *dışından*, kanal üzerinden üretilen raporda doğrulanır; birikim +
  hız penceresi ayrı unit testte. Elenen alternatifler:
  (a) `AtomicU64` küresel kayıt — paylaşımlı mutasyon = §2 ihlali
  ("her değer kanallarla taşınır, paylaşım yoktur"); toplayıcının
  çıktısı mesaj akışının saf fonksiyonu olmaktan çıkar ve hangi
  aktörün hangi sayacı tuttuğu görünmez olurdu.
  (b) Bounded kanal + geri-baskı — bounded `send` Future → tick
  gövdesinde await → "oda tek await'i `tick_rx.recv()`'tir" şartı
  kırılır (spec bunu açıkça yasaklıyor). Örnekler sabit boyutlu ve
  oda başına adım başına en fazla bir tane olduğundan unbounded'da
  geri-baskı fiilen sorun değil; en kötü hal = örnek kaybı, tick asla
  durmaz. (c) Collector'da `tokio::select!` (tick + kanal) —
  gsb-lint `select!`'i yasaklar; ticker aboneliği saat olarak yeterli
  (rapor ≥1 s, tick 30/s). (d) Prometheus tarzı gauge kütüphanesi —
  v1 kapsamı dışında; `MetricReport` zaten dış tüketim için yapısal
  (exporter P2 adayı).

- [x] **Load test harness'i (P0) + ilk uçtan uca sayılar** —
  `gsb-loadgen` binary'si (`crates/gsb-server`, **kalıcı araç olarak
  commit edildi** — spec'in "örnek binary" niyetini `[[bin]]` olarak
  yerine getirir; gerekçe: duman testi `CARGO_BIN_EXE_gsb-loadgen` ile
  spawn'layabilsin ve gerçek `RESULT` satırı üretilsin). Modlar:
  **in-process** (gerçek sunucu `start_server_metrics` ile ephemeral
  portta + N gerçek TCP istemcisi aynı runtime'ta — sunucu tarafı
  metrikler kanaldan, stdout parse'ı **yok**) ve **external**
  (`--addr` ile yalnız istemci tarafı). Yapılandırma: N, `--duration`,
  `--move-ms`, `--room`, `--stagger-ms` (istek i, i×ms gecikmeyle
  bağlanır — 0 = hepsi birden), `--addr`. Duman testi
  (`tests/loadgen_smoke.rs`): binary spawn edilir (3 istemci × 3 s),
  `RESULT` satırı parse edilir — 3/3 connect+join+leave, istemci VE
  **sunucu tarafı** tick hızı 20–40 Hz bandında, snapshot akışı ve
  sunucu byte sayaçları > 0 (suite 3.2 sn; ağırlıklı koşular bilinçli
  olarak suite dışında: spec "normal suite'ü yavaşlatmasın, flaky
  yapmasın" — 10 sn × ölçek başına in-suite koşul bu şartı bozardı).
  Elenen alternatifler: (a) suite içinde N=1000 test — yavaş + flaky
  (CI yükünde); (b) sunucu stdout'unu parse eden dış araç — sunucu
  tarafı metrikleri yapısal olarak yakalayamazdı (hız/histogram),
  log formatına sessiz bağımlılık doğar; kanal sink'i zaten üretim
  sunucunun kullandığı yol. (c) Hazır yük kütüphanesi (k6/locust) —
  gsb frame protokolünü (AUTH/JOIN/MOVE/SNAPSHOT) konuşan istemci
  yine elle yazılacaktı; tek binary in-tree duman testini bir satır
  uzakta tutuyor.
  **Yöntem (dürüstlük cümlesi):** istemciler ve sunucu **aynı
  makinede** (in-process modda aynı runtime'ta; 32 çekirdek,
  release profile, load avg ≈2.5–3.3, somaxconn 4096) çalıştığından
  ölçülen sunucu kapasitesi **alt sınırdır** — istemci işi (decode,
  read loop, tekte 227 MB/s alma) sunucunun CPU'suyla yarışır; ayrı
  yük makinesinde sayılar yukarı çıkar.
  **Ham sayılar (10 s pencere, 30 Hz oda, tek oda, 150 ms MOVE_TO;
  release, 32 çekirdek):**

  | Metrik | N=100 (burst) | N=500 (burst) | N=1000 (burst) | N=1000 (stagger 2ms) |
  |---|---|---|---|---|
  | connected / joined / left | 100/100/100 | 500/500/500 | 1000/**748**/748 | 1000/1000/1000 |
  | hatalı istemci | 0 | 0 | 0 | 0 |
  | connect p50 / p99 | 14ms / 14ms | 1012ms / 1076ms | 1006ms / 1068ms | 0ms / 5ms |
  | snapshot/istemci (p50) | 292 | 268 | 261 | 270 |
  | istemci tick hızı (snapshot `sequence`, median) | 30.00 Hz | 30.00 Hz | 30.00 Hz | 30.00 Hz |
  | **sunucu** tick hızı (raporlar median) | 30.00 Hz | 30.00 Hz | 30.00 Hz | 29.99 Hz |
  | oda adımı (10 s) | 304 | 304 | 304 | 304 |
  | adım süresi p50 / max | 75µs / 240µs | 375µs / 931µs | 3000µs / 4305µs | 3000µs / 2822µs |
  | adım histogramı (µs; [0,50) [50,100) [100,250) [250,500) [500,1k) [1k,5k) [5k,∞)) | [115,147,42,0,0,0,0] | [9,6,42,130,117,0,0] | [8,3,21,9,108,155,0] | [3,3,12,11,18,257,0] |
  | tick gecikmesi max (`late`) | 27µs | 23µs | 37µs | 20µs |
  | **atılan batch** | 0 | 0 | 0 | 0 |
  | lagged event/tick | 0/0 | 0/0 | 0/0 | 0/0 |
  | sunucu çıkış (fan-out, net) | 2.30 MB/s | 58.06 MB/s | 128.84 MB/s | 227.11 MB/s |
  | tepe snapshot payload | 798 B | 4334 B | 6543 B | 8807 B |
  | tepe bağlantı | 100 | 500 | 748 | 1000 |

  **İlk doyma metriği (ölçüm, tahmin değil):** 100 ve 500'de hiçbir
  metrik doyuyor değil — tick hızı iki tarafta da 30.00, drop 0,
  `late` ≈20-27µs, adım p50 bütçenin (33.3ms @30Hz) %0.2-1.1'i.
  N ile **büyüyen tek metrik adım süresi**: 75µs (100) → 375µs (500)
  → 3000µs (1000) p50 — 1000 üyede bütçenin ~%9'u (p99≈3ms, max
  2.8–4.3ms); histogramın ağırlığı [1k,5k) kutusunda. Bu metrik
  fan-out'un maliyeti (snapshot kodlama O(entity) + bağlantı başına
  Arc klonu + `try_send`): N büyüdükçe ilk doyaan şey adım bütçesi
  olacak — doğrusal dışa vurumla (3ms@1000 → 33ms) tek odada
  ~10k üye bütçeyi doldurur; bu tam olarak P2'nin (AOI, oda
  bölme) hedeflediği duvar. Drop 0'ın nedeni: 256 derinlikli
  `conn_out` kanalı + hızlı istemci fan-out'u emer; 227 MB/s
  loopback çıkışı da sorun yaratmıyor.
  **Burst bağlantı bulgusu (spec dışı, dürüst raporlama):**
  N≥~150 **eşzamanlı** connect (loopback) 10 s pencerede joined<N
  yapıyor (koşu bazında 748–956/1000; her koşuda ~1 s'lik dalgalar).
  Kök neden minimal probe ile izole edildi (gsb kodu **yok**, sıfır
  işli ham tokio accept loop): tokio/mio'nun edge-triggered
  (EPOLLET) readiness'i burst'te accept görevini durum-değişimi
  kenarı başına uyandırıyor — kuyruk dolu kalırken yeni kenar
  gelmiyor, istemci ~1 s sonra SYN yinelerek uyanıyor. Aynı
  makinede native (std) accept loop 1000 bağlantıyı **122ms**'de
  kabul ediyor; kernel kuyruğu suçlu değil (backlog 1024, kimse
  accept etmese bile 500 el sıkışma <200ms'de tamamlanıyor); ve
  **stagger'lı** istemcilerle (1ms aralık → 1000/s) hem probe hem
  gsb accept loop sıfır dalga ile yetişiyor. Sonuç: gerçekçi
  kademeli join'de bağlantı kurma sorunsuz (stagger koşusu:
  connect p50=0ms, 1000/1000 join); burst sayıları loopback
  burst'ının tokio accept yolundaki bir artefaktıdır — v1 bulgusu
  olarak raporlanır, actor modeli kapsamında düzeltme yok.


## Kapatılanlar (snapshot turu)

- [x] **Yayın: grup başına tam dünya snapshot'ı** — bağlantı başına
  `last_sent` defteri + `pending_out` + `ENTITY_SPAWNED`/`ENTITY_REMOVED`
  kaldırıldı. `RoomLogic`'e `GroupKey` (Eq+Hash+Clone) + `group_of()` +
  `snapshot()` (false = değişiklik yok; üyelik değişimi dahil) +
  `private()` eklendi (demo `GroupKey = ()`; arayüz `ConnectionId` gibi
  oyuncu başına grubu da destekliyor — core'de testle doğrulandı). Oda,
  tick başına grup başına snapshot'ı BİR KEZ kodlar, `freeze()`'ler ve
  `Bytes` (Arc) klonu olarak dağıtır; `OutSink`/kare tamponlaması kalktı.
  Proto: tekil `EntityState` → paketlenmiş `WorldSnapshot` (entity
  kayıtları + monoton `sequence`); kayıt başına versiyon alanı düştü —
  sıralamayı `sequence` üstleniyor. (Wire boyutu için ölçülmüş sayılar:
  yukarıda "2aad7ea denetim turu" F6 maddesi + DESIGN §8; eski "21→16 B"
  iddiası ölçümden geçmedi: aslı ~18 B → ~12 B.) Değişiklik yoksa yayın
  durur; yapılandırılabilir keepalive (varsayılan 1 Hz, `keepalive_hz`)
  önbellekli snapshot'ı yeniden gönderir (paket kaybeden istemci kalıcı
  bayat kalmaz). `RoomConfig.max_snapshot_bytes` aşımı uyarı loglanır
  (rUDP MTU hazırlığı). **Ölçüldü (release, 1 hareketli + N hareketsiz
  oyuncu, adım p50):** 100: 270→15 µs · 200: 1125→34 µs · 400:
  5359→73 µs · 800: 66151→140 µs (~472×; 800'de 33 ms bütçesinin 2×
  üstündeydi, artık %0.4'ü). Regresyonlar: late-join (ilk yayında tüm
  dünya), stale-leave, kare hızından bağımsızlık — hepsi snapshot
  semantiğine uyarlandı ve yeşil; 2 yeni core testi (grup yalıtımı +
  private, sessizlik + keepalive). AOI/hücre bölme kapsam dışı.

- [x] **Broadcast tick mimarisi** — oda-başına pacer kaldırıldı; tek global
  ticker görevi `tokio::sync::broadcast` ile `TickInfo{tick, at}` yayınlıyor.
  Oda actor'ünün tek await'i `tick_rx.recv()`; tick gövdesi senkron, 5 fazlı
  (CONTROL → READ → CONVERT → SYSTEMS → BROADCAST). Oda hizi global hızı tam
  bölmeli (`run_every`); bölünmeyen hız `CoreError::TickRate` ile reddedilir.
  `Lagged` → uyarı + duvar saati `dt` ile catch-up; `Closed` → temiz çıkış.
- [x] **Per-user action channel** — her bağlantının kendi `Action` kanalı;
  conn actor `try_send` ile iletir (doluyken kendi girdisini atar, izolasyon).
  Kontrol düzlemi (Join/Leave/Shutdown) ayrı control kanalı; join/leave tick
  sınırında işlenir (≤1 tick, deterministik).
- [x] **Kare hızından bağımsızlık + catch-up cap** — `dt` duvar saati
  tabanlı (kaçırılan tick'ler bir sonraki adımda telafi); üst sınır
  `max_catchup = 4` periyot. Test: `gsb-game/tests/frame_independence.rs`
  (60 Hz vs 15 Hz oda, aynı 5.0 s sim süresi → aynı mesafe).
- [x] Küçükler: `ServerHandle` ticker `JoinHandle` + `stop()` kaskadı
  (registry → ticker.abort → accept.abort), config anahtarları
  (`room_control`, `conn_action`; `room_mailbox` kaldırıldı).

- [x] **#1 Join'de tam dünya snapshot'ı** — `last_sent` artık bağlantı
  başına; sonradan giren oyuncu ilk tick'te tüm dünyayı görür.
  Regresyon testleri: `gsb-game/tests/late_join.rs`.
- [x] **#2 `gsb-lint` yeniden çalıştırma + kapsam** — `check()` tarama
  kapsamındaki her dosya için `cargo:rerun-if-changed` yayınlar; kapsam
  `src/`+`tests/`+`examples/`; desenler `select!`/çıplak `Mutex`/`RwLock`
  ile genişletildi; `gsb-ecs` ve `gsb-protocol` lint'e bağlandı (6/7 crate).
  Poison testiyle doğrulandı: src-only değişiklik artık build'i patlatıyor.
- [x] **#3 Registry non-blocking** — join/leave odasıyla olan gidip-geliş
  bağlantı başına **dispatcher görevine** devredildi; registry hiçbir
  zaman oda await etmiyor.
- [x] **#4 Bildirim yolu kaybı + leave/rejoin sırası** — conns kaydı
  bağlantının bütün ömründe yaşar (inbox asla bırakılmaz); `PlayerLeft`
  entity taşır; stale leave (entity uyuşmazlığı) yok sayılır; join,
  bağlantının eski odasını temizleyerek girer. Test:
  `gsb-core/tests/registry.rs`.
- [x] **#5 `RoomId(0)` sentinel** — `ConnInfo.room: Option<RoomId>`.
- [x] **Tick coalescing** (o zaman pacer tabanlı; broadcast geçişinde yerini
  duvar saati `dt` catch-up'ı aldı — "biriken Tick" kavramı yok oldu).
- [x] Küçükler: reader pump double-`Closed`, `RoomConfig::conn_out_capacity`
  ölü kod, accept loop backoff, DESIGN.md iddia düzeltmeleri
  (fan-out O(dirty×conn) dürüst hali, "tek await" formülasyonu, lint kapsamı),
  registry shutdown'da break (kendi mailbox klonu EOF'u engelliyordu).


## Kapatılanlar (grup adilliği turu)

- [x] **Grup başına defter sözleşmesi (per-group state contract)** — dış
  ölçüm: DemoRoom defteri 1:1 kopyalanıp `type GroupKey = ConnectionId`
  yapıldığında (arayüzün reklam ettiği kullanım) 2 bağlantı + her tick
  hareket eden 1 entity + 25 tick'te dağılım **1/26**'ydı (kaybeden
  bağlantı kendi join tick'inden beri yeni snapshot alamıyordu; dağılım
  çalıştırma içi 100/0 — group tablosu `HashMap`'inin ziyaret sırası
  koşu boyunca sabit; kazananın kimliği process bazında
  `RandomState`'ten). Mekanizma: oda her tick'te her grup için
  `snapshot()`'ı birer kez çağırıyor (4c, `groups.iter_mut()`); paylaşımlı
  `last` defteri tek adettediğinden ÖNCE ziyaret edilen grup değişikliği
  tüketip defteri yazıyor, SONRAKİ grup aynı tick'te "değişiklik yok"
  görüyor. Düzeltme katmanı: oda tarafında mekanik koruma imkânsız
  (mantık durumu opak; sessizlik meşru bir hâl) → (1) `RoomLogic::snapshot`
  sözleşmesine açık şart eklendi: karar + defter `group` anahtarıyla
  tutulmalı, çağrı sırası belirsizdir, tek defter yalnız `GroupKey = ()`
  odalarda doğrudur; (2) demo modül/alan dokümanına aynı uyarı; (3) core
  regresyon testi: her tick değişen dünyada aynı tick'te değişen **her**
  grup yayınlanır (grup başına defterli mantıkla, 27/26 payload
  dizileriyle doğrulandı); (4) `GroupKey`'ye `Debug` bound'u.
- [x] **F4: asla yayın yapmamış grup için tanı** — üyesi varken hâlâ hiç
  snapshot üretmemiş grup (ilk tick'te `snapshot` → `false`; üyelik
  değişikliği bir değişiklik olduğundan sözleşme ihlali) oda tarafından
  **bir kez** `warn!` ile loglanır (grup adı + üye sayısı). Sessiz
  (en az bir kez yayınlamış) gruplar tetikleyemez → meşru AOI sessizliğinde
  yanlış alarm yok.
- [x] **F1: demo "değişiklik yok" kararlayıcısı wire içeriğiyle** —
  `last` defteri `(entity → (x, y))` (wire'ın kesilmiş konumları) oldu.
  `bump()` disiplini (önce yalnız yorumdaydı) artık yayın kararını
  etkilemez: `bump()`'sız `Position` yazımı yayınlanır, içeriği
  değiştirmeyen `bump()` yayınlatmaz (ROADMAP'teki "bump() her tick aynı
  tam sayıyı yayınlar — bant israfı" noktası bu tur kısmen kapatıldı;
  kalan parça koordinat formatı kararı). 2 unit test eklendi.
- [x] **F3: `GroupState::members` ölü alanı kaldırıldı** — her
  tick/grup boşa `Vec` alloke edip hiçbir okuma yoktu; fan-out zaten
  bağlantı tablosundan çalışıyor. Grup başına durum artık
  {son snapshot, bu tick'in gönderimi, tanı bayrağı}.
- [x] **F2 (DemoRoom no-change yolunun testsizliği) — denetim turunda
  kapatıldı:** yol artık doğrudan testli — `snapshot_emits_on_plain_
  position_write` (düz `Position` yazımı yayınlanır) + `snapshot_
  silent_when_wire_content_unchanged` (içeriği değiştirmeyen yazım
  sessiz, leave yayınlatır); odaseviye sessizlik + keepalive yeniden
  gönderimi ayrıca `unchanged_group_is_silent_until_keepalive`'da.
- F5 ve F6 (bu turun bulgusu) 2aad7ea denetim turunda kapatıldı
  (yukarıda, "Kapatılanlar (2aad7ea denetim turu)").


## Kapatılanlar (denetim turu — `3f25874` denetimi)

- [x] **Bulgu 1: `late_joiner_receives_full_world_snapshot`'ın
  ConnectionId / `spawn_pos` / `DEFAULT_SPEED` / tick periyodu
  bağımlılığı** — F1 sonrası emisyon wire içeriğiyle (tam sayı
  konum) kapalıyken commit'teki loop'un "tek tick besle → batch'i
  bekle" (bloklama) yapısı, yalnızca ilk hareket tick'inde ızgara
  sınırının geçildiği id'lerde geçiyordu: 199 id taramasında 91'i
  k=1 (geçer), 108'i k>1 (5 sn bloke → `timed out waiting for a
  batch` panic). Düzeltme: `stale_leave` testindeki desene çekildi —
  tick besle, `sleep(10 ms)`, batch **hangi tick'te gelirse**
  `try_recv` ile al; 60 tick bütçesi ve tüm assertler korundu. Kabul
  ölçütü: aynı 199 id'lik oda-seviye tarama düzeltme sonrası
  199/199 geçti (probe commit edilmedi; ham çıktılar raporda).
- [x] **Bulgu 2: `EntityVersion` okuyucusuz kaldı, dokümanlar onu
  hâlâ kullanımda anlatıyordu** — F1 kararı wire içeriğine
  taşıyınca versiyonun tek okuyucusu gitti; yazanlar (spawn'da
  insert, MOVE_TO ingest + her hareket tick'inde `bump`) ve sistem
  (`gsb-ecs::dirty`, `systems.rs`, DESIGN §7, ROADMAP bu maddenin
  atıfı, README crate tablosu) hâlâ aktif mekanizma gibi duruyordu.
  Düzeltme: mekanizma **kaldırıldı** (alternatif "sakla + dokümanları
  düzelt" reddedildi: okuyucusuz yazım hot path'te bedava değil,
  "bump disiplini" artık hiçbir şeyi zorlamaz; gerekirse P2 delta
  yayınında o özellikte yeniden getirilir). `gsb-ecs::dirty` modülü,
  prelude re-export'u, 3 çağrı noktası ve `frame_independence`
  spawn tuple'ı temizlendi; 2 unit test uyarlandı (amaç/assertler
  korundu), biri yeniden adlandırıldı
  (`snapshot_emits_on_plain_position_write`).
- [x] **Bulgu 3: F4 tanısı, motivasyon olan 1/26 aç kalma modunda
  tetiklenmiyordu ve testsizdi** — Ön koşul ölçümü (probe; ham çıktı
  raporda): paylaşımlı defterli mantıkla 16 (A,B) çifti, her iki
  ziyaret sırası da oluştu. Erken girenin aç kaldığı 7 çift =
  ölçülen 1/26 modu (A=1, B=26): **0 uyarı** → ana iddia
  **doğrulandı** (kaybeden grup kendi join tick'inde yayın yapmış →
  `last` dolu → koşul asla tetiklenemez). Geç girenin aç kaldığı 9
  çiftte tick 3'te tam 1 uyarı, grup adıyla. Düzeltme: (1) yeni
  `RoomActor` regresyon testi — asla yayın yapmayan `RoomLogic`
  stub'u + kilsiz (mpsc tabanlı) `tracing` subscriber'ı: uyarı
  **tam bir kez**, grup adını söyleyerek tetiklenir
  (`never_emitted_group_warns_once_naming_the_group`); (2) kapsam
  netleştirildi (DESIGN §4 Tanı maddesi): kaybedeni en az bir kez
  yayınlamış paylaşımlı-defter aç kalması ve tek tick ihlal eden
  grup (join+leave aynı tick'te; `gone` budaması) yakalanamaz — oda
  meşru sessizliği ayırt edemez, koruma sözleşme metnidir. (Hüküm,
  2aad7ea denetim turunda odanın tick başına gözlemi tek tek sayılarak
  yeniden sınandı ve doğrulandı: aç kalan grup ile içeriği gerçekten
  donan meşru grup aynı gözlem serisini üretir — bkz. "Kapatılanlar
  (2aad7ea denetim turu)" F4 maddesi ve DESIGN §4 Tanı.) Yan
  etkiler: uyarı metnindeki "(and every tick since)" kehaneti
  düzeltildi; log alanı için gereksiz `key().clone()` kaldırıldı.
- [x] **Bulgu 4: ROADMAP F2 maddesi güncel değildi** — F1 turu
  eklenen 2 unit test DemoRoom "değişiklik yok" yolunu doğrudan
  kapsarken madde hâlâ "P1 olarak duruyor" derdi. Düzeltme: madde
  kapatıldı ve testlere atıf yapıldı (aşağıda, grup adilliği turu
  bölümü); odaseviye sessizlik zaten
  `unchanged_group_is_silent_until_keepalive`'da.
- [x] **Bulgu 5: DESIGN §11'deki "regresyon" iddiası yanlıştı** —
  yeni core testi paylaşımlı defter yanlış kullanımını yakalayamaz
  (test mantığı grup başına defterli çalışır; oda meşru sessizliği
  ihlalden ayırt edemez). Cümle düzeltildi: test, oda tarafının
  grup-başına davranışına regresyondur; mantık tarafı koruması
  sözleşme metnidir.
- [x] **Diğer doküman hataları (denetimde ayrıca bulundu, aynı turda
  düzeltildi):** README — BROADCAST fazı açıklaması (eski "dirty
  entity → batch + flush"), opcode listesi (eski
  `ENTITY_SPAWNED/REMOVED/STATE`), test sayısı; DESIGN — "önbellek
  hiç dolmadıysa hiç nothing" (karışık dil); gsb-core manifest
  açıklaması ("4-phase tick" → 5 faz).


## Kapatılanlar (2aad7ea denetim turu)

- [x] **F5: `keepalive_hz > tick_hz` sessizce devre dışı kalıyordu** —
  `((tick_hz / keepalive_hz).round() as u64).max(1)` oranı < 1 olunca
  keepalive adımına clamp'leniyor, "değişiklik yoksa sessizlik"
  kazancı tamamen kapanıyordu (tick 30 + keepalive 60 → her adım
  yeniden gönderim); yapılandırmada ilişki kontrol edilmiyordu,
  çalışma anında log/metrik yoktu. Düzeltme: (1) registry `CreateRoom`
  içinde `keepalive_hz > tick_hz` (ve `> 0`) yeni
  `CoreError::KeepaliveRate` ile reddediyor — mevcut `TickRate`
  reddinin aynısı (hız ilişkisi
  yapılandırma invariant'i; sessiz degrade edilmiş oda başlamasın);
  (2) doğrudan inşayı (lib kullanımı) koruyan ikinci katman:
  `RoomActor::new` aynı ilişkide **bir kez** `warn!` veriyor (iki
  hızı da adlandırarak) ve clamp davranışını belgeliyor;
  (3) `config.example.toml` + `RoomConfig` dokümanına `≤ tick_hz`
  şartı. Testler: `create_room_rejects_keepalive_above_tick_rate`
  (120>60 reddedilir; == tick, < tick ve 0 kabul) +
  `keepalive_above_tick_warns_at_construction_and_clamps_to_every_step`
  (doğrudan inşa: uyarı tam bir kez; clamp'li davranış — join
  yayınından sonra HER adım yeniden gönderim, 6/6 batch; meşru
  oranlar == tick ve 1 Hz uyarı vermez). Not: metrik altyapısı yok
  (P0 "Temel metrik" hâlâ açık) — mevcut sinyal log.
- [x] **F6: "21→16 B wire iddiası" ölçülerek düzeltildi** — iddia
  ROADMAP snapshot maddesinde duruyordu (`3f25874` denetim turunun F6
  bulgusu sayıların hatalı olduğunu saptamıştı; `0888441`'in
  sfixed32→sint32 geçişinden sonra iddia tamamen geçersizdi). Gerçek ölçüm (probe; gerçek `RoomActor` +
  `DemoRoom` + kanallar, ham çıktı commit raporunda): yeni `EntityRecord`
  tipik 10 B (3 tag + 5 B entity varint + 2 × 1 B zigzag koordinat;
  koordinat 0 → proto3 default → 8 B kayıt) + snapshot'ta kayıt başına
  2 B çerçeve; toplam: 1 entity = 14 B, 100 = 1196 B, 800 = 9544 B
  (marjinal ~11,93 B/kayıt). Eski `sfixed32` + `version` kaydı aynı
  prost ile 18 B — yani asıl geçiş ~18 → ~12 B, iddia edilen 21→16
  değildi. DESIGN §8'e ölçülmüş sayılar + bevy 0.19 entity-bit notu
  (index bit tersi → varint pratikte her zaman 5 B) eklendi; 1400 B
  eşik ~117 entity'de aşılır. ROADMAP snapshot maddesi düzeltildi.
- [x] **F4 hükmü yeniden incelendi: "oda bu modu tespit edemez"
  doğrulandı** — önceki turda F4 tanısının yalnızca "ömrü boyunca hiç
  yayınlamamış" grubu yakaladığı, paylaşımlı defterli kaybeden
  grubun kendi join tick'inde bir kez yayın yaptığı için tanının hiç
  tetiklenmediği ölçülmüş (1/26 modu, 0 uyarı) ve mod "yapısal olarak
  tespit edilemez" denilmişti. Bu turda hükmün gerekçesi somutlaştırıldı
  ve DESIGN §4 Tanı maddesine taşındı: odanın tick başına gözlemi
  (grup tablosu/`GroupState`, üyelik, önceki tick durumu, `snapshot()`
  dönüşü) tek tek sayıldı; aç kalan grup ile son yayınından sonra
  içeriği gerçekten donan meşru grup (AOI'de tek hareketli entity
  alanı terk eder; demo'da oyuncu hedefine ulaşır) bu gözlem serisine
  bire bir aynı düşüyor → ayırt edici sinyal yok; payload core'a opak
  (core/oyun sınırı), `sequence` bu modda ilerlemiyor, ack yok,
  keepalive `last`'i klonluyor. Hüküm doğru → kod değişikliği
  yapılmadı; doküman notu bu gerekçeyle netleştirildi.


## Kapatılanlar (wire kimliği turu)

- [x] **`EntityRecord.entity` kompakt kimlik: tam bevy bits → oda-yerel
  monoton serial** — F6 turunda ölçülen gerçek durum: kayıt tipik 10 B
  ve bunun 5 B'si yalnızca entity kimliği (bevy 0.19'da `to_bits()`'in
  alt 32 biti `0xFFFFFFFF - index` olduğundan varint pratikte her zaman
  5 bayt; eski nottaki "bit tersi" ifadesi ölçümde yanlış çıktı, aslı
  bit komplementi). 1400 B eşiği ~117 entity'de aşıyordu. Çözüm: oda,
  spawn başına sıradaki wire kimliğini (1'den monoton artan) entity'ye
  `WireId` componenti olarak stamp'lar ve **oda ömrü boyunca bir değeri
  asla yeniden vermez**; aynı değer `JOIN_ROOM_RESULT`'a gider (iki yol
  tek uzayda; `gsb-core`'daki opak `EntityId`'a dokunulmadı — yalnız
  doküman düzeltildi). Kimlik değişmezi nasıl korunuyor: istemcinin
  dünya görüşü son kabul ettiği snapshot'tır (delta/geçmiş/out-of-band
  remap yok); iki farklı entity oda ömrü boyunca aynı tel kimliğini
  paylaşılamayacağından, kimlik hem eski hem yeni snapshot'taysa aynı
  entity (hareket), yalnız yeni snapshot'taysa yeni entity — kayıp
  snapshot olsa bile. Değerlendirilen ve elenen alternatifler:
  (a) yalnız bevy *index* (1-2 bayt) — slot yeniden kullanıldığında
  kimlik çarpışır; bevy 0.19'da yeniden kullanım her 129 free'den sonra
  gerçekleşiyor (ölçüldü: yerel free buffer 128) ve kayıp snapshot
  durumunda istemci yeni entity'yi "eski entity teleport oldu" diye
  okur → değişmez kırılır; ayrıca protokol, bevy allocator'ının iç
  davranışına sessiz bağımlılık kazanırdı. (b) tam bits + başka varint
  kodlaması — alt 32 bit ≈ 2^32 olduğundan 5 baytun altına inemez.
  Ölçüm (gerçek `RoomActor` + `DemoRoom` + kanallar; önceki turun
  yönteminin birebir tekrarı, ham çıktı commit raporunda):
  1 entity = 14 → 10 B; 100 = 1196 → 796 B; 800 = 9544 → 7017 B
  (marjinal ~11,93 → ~8,77 B/kayıt); entity varint histogramı N=1/100:
  5B×N → 1B×N; N=800: 5B×800 → 1B×127 + 2B×673 (serial 128'den sonra
  2 bayta geçer); 1400 B eşiği N=118 (1410 B) → **N=171 (1403 B)**.
  Serial'ın büyüme karakteri: oda spawn sayacı < 128 → 1 B, < 16384 →
  2 B, < 2M → 3 B (uzun ömürlü, çoklu churn'li odalarda tipik kayıt
  9 B). Regresyon: `wire_identity_survives_ecs_slot_reuse` — 129
  join/leave döngüsüyle bevy slot yeniden kullanımını zorlar (geri
  dönen index'in eski entity'nin index'i olduğunu ve generation'ın
  yükseldiğini assert ederek testi boş olmaktan korur) ve geri dönen
  slotun taze wire kimliği taşıdığını doğrular: kayıp snapshot'ları
  olan istemci yeni entity'yi "yeni entity" olarak okur. Mevcut
  e2e/late_join testleri semantiğiyle aynen korundu (join result
  kimliği snapshot'ta aynı uzayda bulunuyor); `snapshot_emits_on_
  plain_position_write` uyarlandı: `on_join` artık serial döndürdüğü
  için bevy handle `conn_entity` üzerinden okunuyor (amaç/assertler
  aynı).


## Kapatılanlar (yayınlanabilirlik turu)

- [x] **Yayınlanabilirlik ön koşulu yorumdaydı — yapısal hale getirildi**
  — `0bf5b79`, snapshot sorgusunu `(Entity, &Position)`'tan
  `(&WireId, &Position)`'a çevirdi ve `WireId` yalnız `on_join` içinde
  atanıyordu: `Position` taşıyan ama `on_join`'den geçmemiş bir entity
  (mermi/NPC/tuzak gibi oyuncuya bağlı olmayan ilk entity) hiçbir
  istemciye **hiç** görünmez olacaktı — hata yok, uyarı yok, sadece
  yok. Bu, aynı dosyada bu sınıfın üçüncü örneğiydi (`bump()`
  disiplini — F1; `GroupState::members` — F3) ve ikisinde de kural
  yalnız yorumda yaşıyordu; bu yüzden "belgele" bu turun çıtası
  değil. **Seçilen çözüm: (a) entity bir sonraki snapshot'ta görünür** —
  broadcast geçişi, `Position` taşıyıp `WireId` taşımayan entity'lere
  odanın **tek monoton sayacından** taze serial damgalar (iki aşamalı:
  yetim sorgusu `query_filtered::<(Entity, &Position), Without<WireId>>`
  ile toplar, damga yazılır, tam sorgu damgalardan SONRA çalışır — her
  entity tam olarak bir kez toplanır; ilk denemede yetim hem damga
  hem tam sorguda toplanınca double-count yakalandı ve düzeltildi).
  **Neden (a), neden (b) "odanın tespit edip bildirmesi" değil:**
  (i) `0bf5b79` öncesi sözleşme "Position ⇒ yayın" idi; (a) bu
  sözleşmeyi yeni kimlik uzayıyla geri koyar, (b) daraltılmış kümeyi
  koruyup yalnız bir log satırı eklerdi — spec'in "sessizce kaybolmak
  kabul değil" çıtasını (b) de karşılar ama motifi (mermi eklendiğinde
  görünmez entity) yaşatırdı. (ii) (b) "spawn'da WireId damgala"
  **disiplinini** yaşatır — bu turun "kural yalnız yorumda yaşamasın"
  çıtasının tam karşısı; gelecekteki mermi geliştiricisi (b)'de
  görünmez entity + uyarı logu alır, (a)'da çalışır entity.
  (iii) Oda-seviye tanı (F4 deseni) yalnız oda **yetkiyi
  kullanamadığında** doğrudur (opak payload, ayırt edilemez gözlem
  serisi); oyun mantığı kendi dünyası üzerinde tam yetkilidir — rapor
  etmek değil, düzeltmek gerekir. **Değerlendirilip elenen
  alternatifler:** (b) tespit + `warn!`, görünmezlik kalsın —
  yukarıdaki üç gerekçeyle; (c) yetim entity'de `expect`/panic — normal
  bir gelecek oyun deseni (mermi) sunucu çöküşüne çevrilirdi ve
  "hata yok, uyarı yok"u "server crash"e çevirmek spek'in işaret
  ettiği yönün tersidir; (d) sorguyu `(Entity, &Position)`'a geri
  alıp bevy bits'i telde taşımak — kimlik turunda **ölçülerek**
  elenmişti (5 B varint + slot yeniden kullanımında kimlik
  çarpışması → kimlik değişmezi kırılır); (e) damgayı `update()`
  (SYSTEMS) fazına koymak — çalışır (tick-içi spawn hep snapshot
  öncesi damgalanır) ama tek boğaz noktası `snapshot()`'ta değil
  fazda kalır: ingest/system/test hangi fazda spawn ederse etsin
  snapshot yakalar, (e) yalnız "bu tick'te update() çalıştı" yolunu
  garanti eder — boğaz noktasını asıl tüketiciye (snapshot) koyduk.
  **Sınıfı kapatır mı: KAPATIR** — "yayınlanabilirlik ön koşulu
  yalnız yorumda yaşıyor" sınıfının demo tarafındaki tüm örnekleri
  bu değişiklikle kod tarafına taşındı: yayın kümesi artık sorgu +
  damga ile tanımlı, yorum değil. Kalan tek "yorumda yaşayan" üye
  grup başına defter şartı (F4; yapısal olarak kapatılamaz —
  aşağıda MADDE 3). Kimlik değişmezi korunur: tek sayaç, monoton,
  asla yeniden verilmez — `wire_identity_survives_ecs_slot_reuse`
  hâlen geçerli ve yeni atama noktası aynı sayaçtan beslenir.
  **Regresyon (seçimi kilitleyen test):**
  `entity_spawned_outside_on_join_is_broadcast_with_fresh_wire_id` —
  entity `on_join` **olmadan** doğrudan world'e spawn edilir;
  snapshot'ta görünür (taze serial 3), damga idempotent (içerik
  değişmezse sessiz + kimlik sabit), hareket sonrası **aynı** kimlik
  yeni konumda. **Tel boyutu etkisi: ölçüldü, SIFIR** (probe; gerçek
  `RoomActor` + `DemoRoom` + kanallar; probe commit edilmedi):
  N=1: **10 B** (8,00 B/kayıt) · N=100: **796 B** (7,94) · N=800:
  **7017 B** (8,77) · 1400 B eşiği **N=171**'de aşıldı (1402 B; N=170
  = 1400 B → eşik aşılmaz). Öncesi (`0bf5b79` worktree, aynı probe)
  ile sonrası **bire bir aynı** — kararlı durumda yetim sorgusu boş
  (O(0)) ve payload kayıt-kayıt aynı. Önceki turun kayıtlı "1403 B"
  değeri bu probe'un "1402 B" değerinden 1 bayt farklı: bu probe tüm
  join'leri tek tick'te işler (`sequence=1` → 1 B varint); eski probe
  snapshot'ı `sequence ≥ 128`'de aldığında 2 B varint olur — eşik
  N'si (171) değişmez. Sayılar bağımsız modelle de (spawn formülü +
  proto3 kodlaması) bire bir doğrulandı.
- [x] **`Owner` ölü durumu kaldırıldı** — `Owner(ConnectionId)` her
  `on_join`'de yazılıyor ama hiçbir yerde okunmuyordu — birebir
  `GroupState::members` (F3) kategorisi: yazılıp okunmayan durum.
  "Gelecekteki mermi/NPC sistemleri shooter'a ihtiyaç duyarsa kalsın"
  alternatifi **reddedildi**: (i) spekülatif — kod tabanının kendi
  emelesi (F1/F3/Bulgu 2): okuyucusuz yazım hot path'te bedava değil,
  ölü durum silinir ve **ilk gerçek okuyucuyla birlikte** geri gelir;
  (ii) `conn_entity` haritasının (conn → entity) aynısını ters
  yönde ikinci bir kaynakta tutuyordu — iki sahiplik kaydı, biri
  ölü; (iii) 8 B/entity + spawn'da yazım bedeli. Bir okuyucusu
  YOK; gerekirse gerekçesiyle geri getirilir.
- [x] **MADDE 3 taraması — aynı sınıftan başka kalıntı** — son iki
  turun değişikliklerinin (wire kimliği, wire içeriği detector'ı)
  ardında tüm workspace "yalnızca yorumda yaşayan ön koşul" / "yazılıp
  okunmayan durum" diye tarandı (alan-bazında read/write sayımı +
  doküman taraması):
  1. **`TickCtx.room` yazılıp okunmuyor** — oda actor'ü doldurur;
     hiçbir mantık okumaz. **Korundu**, gerekçe: `RoomLogic`'in
     halka API'si (pub alan) — Owner'dan farkı: oyun-özel component
     değil; oyun mantığının kendi oda kimliğini öğrenmesinin **tek**
     yolu (ikinci kaynak yok, duplikasyon yok) ve gerçek
     çok-odalı mantıkta doğal okuyucusu var. Bugün okunmaması
     demo sadeliği; ölü durum değil.
  2. **`CoreError::RoomFull` hiç kurulmuyor** — pub varyant, P1
     `max_players` maddesinin taşıyıcısı (rezerve API yüzeyi —
     `PRIVATE` op'undaki "reserved, demo kullanmaz" deseniyle aynı
     sınıf). Kod değişiklığı yok; ROADMAP P1 maddesindeki "yeniden
     eklenecek" ifadesi güncellendi (zaten mevcut — maddenin ifadesi
     eskiydi).
  3. **Stale "4-phase tick" doküman kalıntıları (2 dosya) düzeltildi**
     — denetim turu `gsb-core` manifest tanımını 5 faza çevirmiş ama
     `gsb-core/src/lib.rs` ve `gsb-server/src/lib.rs` modül
     yorumlarındaki "four-phase/4-phase" kalmıştı; "mevcut olmayan
     mekanizmayı anlatan doküman" sınıfından iki kalıntı, aynı turda
     kapatıldı.
  4. **Grup başına defter şartı — sınıfın bilinen ve yapısal olarak
     kapatılamaz tek üyesi** (F4; 2aad7ea turunda odanın tick-başına
     gözlemi tek tek sayılarak yeniden doğrulandı): oda, aç kalan
     grup ile içeriği gerçekten donan meşru grubu ayırt edemez →
     koruma sözleşme metnidir (DESIGN §4 Tanı). Yeni değil; kaydı
     burada tutulur, bu turda değişmedi.
  5. **"Wire kimliği yalnız oda sayacından atanır" bir konvansiyon** —
     modül dokümanı + proto yorumunda yaşar. Bugünkü **tüm** atama
     noktaları (`on_join` + yeni broadcast damgası) tek sayaçtan
     geçer (grep ile doğrulandı); slot yeniden kullanımı tehdidi
     `wire_identity_survives_ecs_slot_reuse` ile kilitli. Yapısal
     olarak kapatılamaz: component tipi allokatörü sahiplenemez,
     crate-içi manuel `WireId(42)` ataması (test/bozuk mantık)
     alan görünürlüğüyle de önlenemez — bu, tesadüfi "eksik ön
     koşul" değil, **bilinçli kötü kullanım** sınıfı (sınıfın
     kapsamı dışında). Not: bu turun damgası ikinci atama noktasını
     ekledi — her ikisi de aynı sayaç, değişmez bozulmadı.
  6. `TICK-ARCHITECTURE.md`'deki "4 faz" tablosu: dosya başlığı
     gereği **tasarım tartışmasının arşivi** — tarihsel anın doğru
     betimi, dokunulmadı.


## Kapatılanlar (ihlal bütçesi + rUDP turu)

Bu tur iki bölüm: **A — protokol-ihlal bütçesi** (cevap amplifikasyonu
sınırı; istemci geliştirici tanısı) ve **B — rUDP taşıması**
(DESIGN §6'daki "aktör katmanında değişiklik sıfır" iddiasının
yapısal sınavı: UDP'de "bağlantı" bir socket değil, datagram
akımlarından sentezlenen oturumdur). Makine: 32 core / 124 GB /
rustc 1.95.0 (koşular release; in-proc yük ölçümleri istemcilerle
server'ı aynı CPU'da paylaşıyor).

### A — Protokol-ihlal bütçesi

Spec'in iki sorusu: (1) amplifikasyon sınırı nasıl konur, (2) istemci
geliştirici neden reddedildiğini anlasın. Cevap: **bağlantı
actor'ünün YEREL durumunda** bir sayaç (kanal yok, görev yok,
registry bilmez; bağlantı ölür sayaç ölür):

- **Bütçe: ömür boyu toplam 16 ağırlıklı puan** (sınıf ağırlıkları:
  Hard = 4, Race = 1, sunucu tarafı koşullar = 0). *Neden kayan
  pencere değil:* ilgili pencere bağlantı ömrüdür — istemci ya
  düzelir ya ölür; churn yapan istemcinin penceresi zaten sıfırdır
  (yeni bağlantı = yeni bütçe, bu zaten yeniden-auth'un doğal
  sonucu). Kayan pencere = bağlantı başına zamanlama durumu +
  per-tick iş — sıfır kazanç.
- **Ağırlık sınıfları:** Hard (bilinmeyen opcode, decode hatası,
  auth ihlali = 4) → 4 ihlahta kapanır; Race (NotInRoom = 1) →
  meşru yarışlar (oda tick sınırında ölüm, leave→rejoin) bütçeyi
  neredeyse çizmez (16 race gerekli); sunucu tarafı koşullar
  (cap/dolu oda = 0) → istemcinin suçu değil, hiç sayılmaz.
- **İlk 3 cevaplanır, sonra susulur:** istemci geliştiriciye 3
  hata mesajı gider (tanı yeterli), sonra ERROR hunisi susar —
  amplifikasyon sınırlı kalır (50 reddedilen istemci = 150 ERROR
  toplamı; sınırsızda 1550+ olurdu; ölçüm: 150 istemci load'da
  `errors=150`, tur öncesi sınırsız davranış ~4×'i).
- **Kapanma:** bütçe tükenince ERROR 9 + mesajda "violation" +
  temiz cleanup kaskadı (mevcut huni — yeni kapatma yolu yok).
- **Peer adresi TAŞINIR:** `Endpoint::peer()` → connection
  actor'ü → kapanma WARN satırı (`self.conn=… self.peer=…`) —
  operatör satırı firewall/fail2ban'a doğrudan verebilir; v1'de
  ban listesi yok (spec bunu kapsam dışı ilan etti — burada yalnız
  SİNYAL kuruldu, KULLANIMI kurulmadı).

**Elenen alternatifler:** (a) kayan pencere (yukarıda); (b) eşit
ağırlık (race'ler Hard ile aynı ağırlıkta → meşru churn'li
istemciler bütçeyi eritir, saldırganla ayrımı zayıflar); (c) her
ihlale cevap (amplifikasyon = spec'in başlıca sorusu, sınırı
yok); (d) adresten yoksun sinyal (operatör elini kolunu bağlar —
peer zaten actor'de, taşıma bedelsiz).

**Ölçüm (150 istemci, room-full, in-proc, release):**
`joined=100 … errors=150 join_rejected=50 cap_rejected=0
budget_rejected=50 actions_dropped=0`; kapanma kaydı
kendine yeter: `closing connection: protocol violation budget
exhausted self.conn=c64 self.peer=127.0.0.1:52116 violations=16
answered=3 score=16`.

**Spec düzeltmesi (not):** spec, bilinmeyen opcode'ların zaten
sınırsız ERROR ürettiğini varsaydı; gerçek actor'de kayıtsız
**temel bant** op'leri (10..999) odaya FORWARD ediliyordu —
buda turda `UnknownOpcode` dalı eklendi (10/11 = UDP taşıma
işaretleri belgeli; TCP'de bunlar bilinmeyen op = bütçeli).

### B — rUDP taşıması

**Yapısal fark:** TCP'de `accept()` kernel'in bitirdiği bir
bağlantıyı teslim eder; UDP'de bir soket **tüm** oturumları
taşıyor, dolayısıyla "accept" datagram akımından oturum
sentezlemektir. Seçilen topoloji (DESIGN §6'da detaylı): **tek
ortak demux görevi** (listener'ın, `bind()`'de başlatılır) tüm
datagramları okur ve peer adresiyle per-oturum mailbox'lara
route eder; her oturum yalnız **writer** görevi taşır (reader
yok). `accept()` = el sıkışma tamamlanınca demux'un ön-oluşturduğu
oturum endpoint'lerinin (crossbeam bounded(1024)) akışı.

**100k matematiği (seçilen demux):** datagram başına = 1
`recv_from` syscall + 1 hash lookup + 1 `BTreeSet` insert
(O(log 100k) ≈ 17 adım) + 1 `try_send` ≈ 0,5 µs → 100k oturum
× 2 datagram/sn ≈ **0,1 core**; oturum başına 10 datagram/sn
(`all` görünürlüğünün tavanı; 3M datagram/sn) ≈ **1,5 core** —
TCP'nin per-connection writer syscall yüküyle aynı mertebeye.
Asıl 100k duvarı çekirdeğin tek-socket pps'si + fan-out hacmi
(görünürlük) — TCP ile aynı sınıf, başka değil.

**Elenen alternatifler (matematik):**
- **SO_REUSEPORT shard'leme** (N soket, N demux, kernel hash):
  tek demux CPU'sunu ~1,5 → 1,5/N core'a indirir — ama toplam
  pps tavanı aynı kaldığı için kazanım **%5'ten az**; ve üç yapı
  kırılır: tek `accept()` akışı (shard başına accept = registry'nin
  tek-accept akış modeli), ortak monoton `ConnectionId` alanı
  (shard başına sayacı = kimlik değişmezinin kırılması), ortak
  idle heap (shard başına saat).
- **Per-due O(N) sweep** (deadline'a kadar bekle, tabloyu tara):
  heap'in O(log N) insert/remove'u yerine O(N) tarayış — join/
  leave churn'ında (100k'da her join/leave bir girdi) 80× maliyet;
  ayrıca "sıradaki due"yu bulmak için de tarama gerekir.
- **`Listener::accept` içine gömülü demux:** `accept`'in döndürdüğü
  future, demux'un kendisini (soketi) borçlanması gerekir →
  self-referential future → 'static stream için unsafe/mio
  seviyesinde iş — kanal yasağı + `unsafe_code="forbind"` ile
  çıkmaz.

**Zorunlu özellikler (hepsi yapıldı):**
1. **El sıkışma cookie/token:** stateless 3-message cookie
   (`HELLO{nonce,0}` → `HELLO{nonce,F(nonce,peer,key)}` →
   `HELLO{nonce,cookie}`); `F` = splitmix64 katlaması, key proses
   başına duvar saatinden. Oturum durumu **yalnızca** son mesaj
   doğrulanınca kurulur (asıl hedef: handshake amplifikasyonu —
   sahte proof key bilmeden üretilemez; oran ≤ 1 çünkü iki
   yöndeki mesaj aynı boyutta).
2. **Band ayrımı:** kontrol bandı (AUTH/JOIN/LEAVE/HEARTBEAT =
   `op 1..=64`, 11 hariç) REL: `[u32 seq][u16 op][payload]` +
   cumulative ACK + RTO 50 ms yeniden gönderim (vazgeçme 250 ms,
   sayılır) + 16'lık out-of-order penceresi, **sıralı teslim**.
   Oyun bandı (`op ≥ 1000`) RAW: `[u16 op][payload]` — kayıp
   toleranslı, sırasız (snapshot'lar tam durum; MOVE_TO'nun kaybı
   bir sonrakiyle örtülür).
3. **MTU:** `max_datagram_bytes` varsayılan 1472 (1500−20 IP−8
   UDP). Bütçe üstü **çıkan** datagram: **atılır + sayılır +
   oturum başına tek uyarı**. *Neden parçalama değil:* parçalama
   bir protokol (assembly penceresi, frag timeout, parça kaybında
   tüm fragment'ı bekleme) — v1 kapsamı (aşağıda); *neden ret
   değil:* ret, gönderen aktöre geri sinyal gerektirir (yeni
   kanal/yön) ve canlı oturumu kırar; drop+count, drop'un nerede
   olduğunu sayılarla gösterir (loadgen'de `oob_dropped`, oda
   tarafında zaten `snap_overflows`).
4. **Oturum kapatma (FIN yok):** demux'un
   `BTreeSet<(Instant, SocketAddr)>` deadline heap'i (gömlekli
   geçersiz kılma: girdi yalnız `son_görülme + idle`'e eşkenken
   geçerli — last_seen güncellenince eski girdi self-expire olur)
   + `timeout(min_deadline, recv_from)`; sweep oturuma
   `ConnIn::ServerClosed` yollar + oturumu kaldırır. Her frame
   last_seen'i sıfırlar (TCP'nin reader-pump saatine birebir
   karşılık). `idle_timeout` config'den (vars. 30 sn).

**Trait sızması (DURUMU VE NEDENİ — dürüst cevap):** `gsb-core`
**dokunulmadı** (odanın tek await'i `tick_rx.recv()`, connection
actor'ün tek await'i `inbox.recv()`, registry oda asla await etmez —
hepsi korundu). Sıza `gsb-net` trait şeklinin **4** yerinde
oldu, her biri yapısal (alternatifi yoktu):
1. `Endpoint::take_inbox`/`take_outbox` (+`with_inbox/with_outbox`):
   demux, oturum mailbox'larını el sıkışmada — accept loop
   çalışmadan **önce** — kurmak zorunda (datagramlar o anda
   gelebilir); TCP'de reader, accept'in içinde kurar.
2. `start_pump`/`PumpSpawner` artık `(Option<JoinHandle>,
   JoinHandle)` döndürür: UDP'de per-connection reader yoktur
   (ortak demux); reader handle'ı `Option`.
3. `Listener::close(&self)` (§9'un ertelenmiş kapısı kullanıldı):
   demux tüm bağlantılardan uzun yaşar — `Arc`'lerin düşmesi onu
   durdurmaz; `stop()` onu abort eder.
4. Peer adresi endpoint'ten aktöre taşınır (`Endpoint::peer()` →
   `ConnectionActor.peer`) — ihlal sinyali için (Bölüm A).
   gsb-server tarafı: `Config.transport` + `udp_max_datagram_bytes`
   + **`crossbeam-channel` bağımlılığı** — `Endpoint`
   göndericisi/`Receiver`'ı `Arc<dyn Listener>`'den
   `recv`/`try_recv` **`&self`**'den çalışmalı (tokio mpsc'nin
   `recv`'i `&mut` ister; kilit eklemek yasak, `unsafe` yasak →
   crossbeam'in atomik iç yapısı tek yol; tek kullanış noktası bu
   akış).

**Kabul kriteri — e2e aynı niyetle her iki taşımada:** `e2e.rs`
`Client`/`Recv` soyutlaması üzerine yeniden yazıldı; 7 akışın
hepsi `for kind in [Tcp, Udp]` ile koşuyor (7 test × 2 = 14 akış,
test sayısı 7). UDP tarafının niyet farkları belgeli: "kapandı"
EOF ile değil **probe başarısızlığı** ile kanıtlanır (HEARTBEAT →
1,5 sn içinde HEARTBEAT_ACK yok = kapandı); `recv` penceresi
"sessiz pencere" = `Ok(None)` (hata değil — UDP'de EOF yoktur).

**Ölçüm (loadgen `--transport udp`, 150 istemci, 10 sn, in-proc,
release, `all` görünürlük, max_players 100) — TCP karşısına:**

| metrik | TCP | UDP |
|---|---|---|
| connected / joined | 150 / 100 | 150 / 100 |
| connect p50 / p99 | 27 / 55 ms | **19 / 35 ms** (handshake) |
| snapshot toplamı | 29 159 | 29 100 |
| measured hz (medyan) | 30.06 | 30.01 |
| adım mean / p50 / max | 81.3 / 130 / 257 µs | 100.5 / 130 / 402 µs |
| adım bütçesi aşımı | 0 % | 0 % |
| client in (ort) | 2 217 KB/s | 2 248 KB/s |
| server out (ort) | 2 243 KB/s | 2 239 KB/s |
| out_bps_per_conn | 15 314 | 15 286 |
| late_max / dropped / actions_dropped | 3 091 µs / 0 / 0 | **1 148 µs** / 0 / 0 |
| istemci yeniden gönderimi (retrans_out) | — | 769 / 10 sn |
| dup_in / oob_dropped / gave_up | — | 0 / 0 / 0 |

Okuma: snapshot bant genişliği ve tick hızı eşdeğer (aynı oda);
handshake TCP connect'inin **altında**; adım mean/max UDP'de
biraz yatakta (demux + per-session writer, in-proc CPU paylaşımı
altında — 4 adım p99 binine; bütçeyi aşan yok); late_max UDP'de
**3× daha iyi** (1 148 vs 3 091 µs — demux'un tek okuyucusu, TCP
reader'larının per-connection wake-up'ı yok); loopback'te sıfır
kayıp; 769 yeniden gönderim = in-proc baskı altında RTO'nun
görevini yapması (kontrol bandı sıralı teslimi bozmadan
kurtardı).

**v1 kısıtları (kapsam dışı — burada ilan, inşa yok):**
congestion control (UDP'de pps sınırını yalnız oda bütçesi
tutuyor; P1: token bucket), şifreleme (cookie key'i duvar
saatinden — yerel saldırgana öngörülebilir; P1: DTLS/TLS katmanı),
parçalama (bütçe üstü = drop+count; `snap_overflows` sinyaliyle
grup bölme asıl kaldıraç — §8), SO_RCVBUF ayarı (tokio 1.53.1
setter'ı yok), NAT yeniden bağlanması = yeni el sıkışma + yeni
`ConnectionId` (eski oturum ≤ `idle_timeout` yaşar), oturum
actor ölümünden sonra ≤ `idle_timeout` kadar demux'te yaşar
(demux'un `in_tx` + registry klonu kanalı canlı tutar; idle sweep
temizler).


## Kapatılanlar (oda segmentasyonu turu)

Bu tur iki bölüm: **A — rUDP cookie key** (el sıkışma anahtarının
kaynağı; sessiz zayıf geri düşüşün kaldırılması — rUDP turunun
güvenlik devamı) ve **B — oda segmentasyonu (`sharded`)** (tek odanın
N shard actor'üne bölünmesi +
sınır migrasyon protokolü; §14.3/14.4'te "kaldıraç / katman
eklenmeden sığmıyor" olarak notlanan sınıfın ilk uygulaması).
Makine: AMD Ryzen 9 7950X 16C/32T (SMT), 124 GB, rustc 1.95.0, tokio
1.53.1. Yük koşuları **ayrı proses** (`--orchestrate --pin`), sunucu
8 fiziksel core'a pin'li (16 HW thread), istemciler ayrı process'ler;
koşular release.

### A — rUDP cookie key (OS entropisi)

**Problem.** rUDP el sıkışmasının cookie key'i proses başına **duvar
saatinden** türetiliyordu → yerel saldırgana (saat + pid bilen)
öngörülebilir → sahte proof üretilebilir. v1'de kriptografik katman
(HMAC) yoktu, ama key'in kendisinin tahmin edilemez olması, sahte-
proof/amplifikasyon korumasının *gerçek* dayanağı olmalıydı.

**Çözüm (seçilen).** Key, 16 bayt, iki kaynaktan biriyle kurulur:
(1) `Config.cookie_key: Option<[u8;16]>` verilmişse o (operatör
denetimi — deploy'da sabit anahtar isteyenler), (2) verilmediyse
`getrandom` ile **OS entropisinden** (`getrandom(2)`/`/dev/urandom`)
16 bayt. **Sessiz zayıf geri düşüş yok**: entropi kaynağı başarısız
olursa yapı `Result::Err` döndürür, süreç başlatmayı reddeder; duvar
saati gibi tahmin edilebilir bir değer asla kullanılmaz. `forged_
proof_is_rejected` testi: rastgele / sıfır / sabit key'li sahte
cookie reddedilir, oturum kurulmaz (+ 2 cookie akış testi).

**Elenen alternatifler.**
1. *Key'i duvar saati + pid'den türet, HMAC ekle*: HMAC katmanı ayrı
   bir protokol yüzeyi (imza doğrulama, key senkronu) demek; v1
   kapsamı dışında. Key'i entropiden almak aynı korumayı (tahmin
   edilemezlik) sıfır ek protokol yüzeyiyle verir.
2. *Key'i her oturumda istemciye sormak (pre-shared)*: stateless cookie
   modelini bozar (istemsiz oturum ön-oluşturulamaz), NAT reconnect
   akışını kırar; operasyonel yük (anahtar dağıtımı) gereksiz.

### B — Oda segmentasyonu (`sharded`)

**Ölçülen soruna (B0 karar koşusu).** C1 duvarı (tek oda, `all`,
ayrı proses): p50 adım 33,3 ms bütçeyi **9k-10k arasında** aşıyor,
10k'da `server_hz` 23,2'ye düşüyor. Soru: bu duvar çoklu oda ile mi,
yoksa tek odanın *içinde* segmentasyonla mı aşılır? Cevap (ölçümle):
**oyun sınıfına göre.**

- **Bölünebilir oyunlar** (MOBA takımları, ayrı lobiler, bölgeler):
  dünya zaten doğal odalara ayrışır → **çoklu oda yeter** (her oda
  bütçe içine sığar, bağlantılar odalara dağılır). Segmentasyon
  gerekmez, daha ucuzdur.
- **Sınırız tek dünya** (sürekli açık dünya, oyuncular bir dünyada
  dolaşır): oda sınırına takılamaz → **segmentasyon gerekir**
  (`sharded`). Bu tur bunu kurar.

Yani segmentasyon *tek sürekli dünya* sınıfına aittir; ölçüm, çoklu
odanın bölünebilir sınıfta yeterli olduğunu (2500 entity'li oda
adımının ~1 ms altı, bütçenin çok içi) ve tek dünyanın segment
gerekttiğini gösterir.

**Mimari seçim (neden `RoomLogic`'in altı değil, topoloji düzeyi).**
`sharded`, tek oda → N actor değişimidir; bu, `RoomLogic` trait'i ile
ifade edilemez (trait tek `World` + tek tick gövdesi varsayar).
Çözüm: `gsb_core::shard` (topoloji-agnostik shard actor + migrasyon
protokolü) + `gsb_game::sharded` (oyun-level `ShardLogic`: grid,
bölge, border, wire range) + registry `BuiltRoom::{Single, Sharded}`
(1→N actor topolojisi). Registry shard'ları **takip etmez**
(bağlantının şu anki shard'ını bilmez) — bu, shard'lar arası
kayb-ekleme (lost-update) durumunu önler.

**Tasarım kararları + elenen alternatifler.**

1. **Partition / AOI hizalama.** Shard'lar haritayı grid'e böler
   (`grid_shape`, en kareye yakın rows×cols, N≤16); bölge testi AOI ile
   aynı (`shard_at`). Elenmiş: (a) *dairesel/radial partition* — sınır
   uzunluğu ve komşuluk düzensiz, migrasyon yolu karmaşık; grid 4-komşu
   ile düzgün. (b) *hiyerarşik (quadtree)* — dinamik yeniden
   dengeleme gerekir, shard'lar arası migrasyon ağacı boyunca; grid
   statik ve basit (v1).
2. **Migrasyon — tek sahip değişmezi** (hiçbir tick dizininde entity
   iki shard'da da yok, hiçbirisinde de yok). Gönderen `Migrate`'i
   *başarıyla* `try_send`'ederse `t+1`'de despawn; başarısızsa mesaj
   geri alınır, bağlantı geri sarılır, entity kalır. Alıcı *install
   gate* ile `at_tick+1`'de spawn (erken gelen ertelenir). Elenmiş:
   (a) *write-through (her iki shard'da bir tick çift sahip)* —
   çift snapshot / çift migrasyon, kimlik çakışması; "tek sahip"
   değişmezi kırılır. (b) *2PC (prepare/commit)* — shard'lar arası
   await/round-trip, tick gövdesini senkron olmaktan çıkarır;
   `try_send` başarısı tek fazda aynı garantiyi verir.
3. **Sınır görünürlüğü (1-tick hizalama).** Komşular `BORDER` fazında
   tam durum değişir; shard komşunun sınır entity'lerini *borrow* olarak
   ekler (kendi kaydı 1-tick-geri borçlu kopyayı yener). Marj
   **çeyrek hücre** (`min(cell_w,cell_h)/4`). Elenmiş: (a) *tam hücre
   marjı* — dejener: s×s hücredeki her nokta bir kenardan ≤s uzakta
   olur, **bütün shard** export olur (borrow kümesi sınırsızlaşır).
   (b) *sıfır marj (sadece sınır hücresi)* — sınırın iki tarafını
   kapatmaz, entity sınırda görünmez olur (lag-blink yerine kayıp).
4. **Bağlantı sahipliği.** Migrasyonda **kanallar entity'yle gider**
   (`Migrate` mesajı `out`/`in` mailbox'larını taşır); yeni shard
   snapshot'ı doğrudan o bağlantının writer'ına yollar. Elenmiş:
   (a) *registry'nin shard'ı takip etmesi (bağlantı → shard yönlendirmesi
   migrasyonda güncellenir)* — shard'lar arası güncelleme = kayb-ekleme
   (bounded kanal + eşzamanlı migrasyon/leave); registry'nin shard'ı
   bilmemesi bu sınıfı tamamen ortadan kaldırır. (b) *bağlantının
   shard'lar arası "forward" etmesi* — her snapshot shard → registry →
   shard round-trip, ek gecikme + kanal.
5. **Wire kimliği (range partitioning).** Shard `i` → `i * 2^20 +
   serial` (`SHARD_SERIAL_RANGE = 1<<20`); id migrasyonda değişmez
   (entity `WireId`'siyle gider). Elenmiş: (a) *global (paylaşımlı)
   id sayacı* — join senkron tick gövdesinde koştuğu için global "sonraki
   id" shard'lar arası senkronizasyon ister; range partitioning sıfır
   senkronizasyonla çakışmasızlık verir. (b) *her shard kendi 0'dan
   sayar (id + shard ön eki)* — istemci kimliği shard'la bileşke
   olur, migrasyonda id değişir (istemci dünyasında kimlik bozulur).
6. **Leave / migrasyon yarışı — iki-tablo epoch şeması.**
   `conn_epoch` = shard'ın şu an tuttuğu join'in epoch'u (Migrate-out
   epoch taşımak için); `conn_tombstone` = işlenen en yüksek LEAVE
   epoch'u. Kapı **tombstone**'a bakar (`tombstone >= p.epoch` →
   reject), *kurulu* epoch'a değil. Elenmiş: (a) *kurulu (`conn_epoch`)
   tablosuna kapı* — ping-pong migrasyonu aynı join'in epoch'unu taşır;
   kurulu tablo onu "bilinen" sayıp **canlı join'i reddederdi** (test 1,
   tick 14'te "wire 1 owned by []" ile yakalandı). (b) *registry'nin
   leave'i shard'a iletmesi (yönlendirmeli)* — registry'nin shard'ı
   bilmesi gerekir (karar 4'e aykırı); leave'in **tüm** shard'lara
   `max` birleşmesiyle yayılması (epoch 0 ile, zararsız) daha basit ve
   kayıpsız.
7. **`&World` metotları → cache.** `collect_border`/`own_wires` `&self`
   + `&World` aldığı için (borrowed context) canlı sorgu yapamaz
   (`&mut World` isterlerdi) → `border_cache` (update sonunda yeniden
   kurulur; migrate-out'a karşı 1-tick-geri **zararsız**, alıcının
   `own_wires` filtresi bayat kopyayı düşürür) + `own_wires: HashSet`
   (her mutasyonda senkron — bayat kayıt, komşunun artık-düzgün
   kaydını gizleyip entity'yi 1 tick düşürür).
8. **Kapasite.** Cap oda düzeyinde **tama** (`members + pending >=
   cap`); shard başına `ceil(cap/N)` değil (range-partitioned join,
   cap'in dağılımını bilmez).

**Doğrulama (unit + entegrasyon; hepsi yeşil).**
- `gsb_core::shard` (5 test): `migration_never_drops_or_duplicates`
  (ping-pong, her tick dizininde tam-tek sahip + süreklilik),
  `wire_identity_stable_and_disjoint` (id stabil + shard'lar arası
  çakışmasız), `in_flight_action_survives_migration` (uçuştaki aksiyon
  migrasyonda kaybolmaz), `ghost_migrate_after_leave_is_rejected`
  (iki-tablo epoch: leave sonrası hayalet migrasyon reddedilir),
  `boundary_entities_are_visible_to_both_sides` (sınırda iki oyuncu
  birbirini görür).
- `gsb_game::sharded` (6 test): `region_partition_tiles_the_map`,
  `wire_ranges_are_disjoint_and_stable`,
  `migration_reports_crossing_with_full_state`,
  `border_visibility_across_the_seam`,
  `frame_filter_discards_far_neighbor_edges` (N=16: sınır entity'si
  görünür, uzak-kenar entity'si frame filtresinde elenir),
  `snapshot_union_and_no_change`.
- Loadgen: `--visibility sharded --shard-count N` (in-proc + `--serve`
  + `--orchestrate` üç modda da shard_count akıtıldı); rapor
  **shard-aware** (`fold_rooms`: tek oda = özdeş, sharded = shard'lar
  üstü toplam/en-kötü-shard) + RESULT'a `shards=N` alanı.

**Ölçüm (ayrı proses, `all`, 30 Hz, sunucu 8 core pin'li, 10000
bağlantı, stagger'lı join — temiz steady-state):**

| Konfig | peak_conns | server_hz | adım p50 | adım max | over-budget | server_cpu_s |
|---|---|---|---|---|---|---|
| C1 (1 oda, `all`) | 10 000 | 30.02 | 6 250 µs | 73 279 µs | **%3,4** | 80,8 |
| `sharded N=4` | 10 000 | 29.93 | 6 250 µs | 72 956 µs | **%0,2** | 111,4 |
| `sharded N=8` | 10 000 | 29.93 | **782 µs** | 35 818 µs | **%0,0** | 117,8 |

8k'da fark daha belirgin: C1 `over_budget %7,7` / `step_max 146 831 µs`
(4,4× bütçe) vs `sharded N=4` `%0` / `31 305 µs`.

**Yorum.** (1) **Adım-duvarı (tick bütçesi) segmentasyonla kayıyor:**
tek oda 10k'da bütçeyi aşarken `sharded` aşılmıyor; N arttıkça p50
adım düşüyor (N=8 ~8×). (2) **Fiyat:** shard protokolü (kanal trafik +
border değişimi + migrasyon + N× actor) sunucu CPU'yu ~1,4-1,5×
artırıyor (80,8 → 117,8 core-s; pin'li havuzun ~%22 → %32'si) — sunucu
hâlâ doygun değil, dolayısıyla CPU henüz yeni duvar değil. (3)
**Fan-out/decode duvarı segmentasyonla kaymaz:** `dropped`
(snapshot'ların istemci kanalında atılması) üç konfigde de yüksek
(2,3-4,0M) ve istemci decode'u ile sınırlı (`clients_cpu_s≈400`) — bu,
loadgen istemcisinin 10k×30Hz×~60KB snapshot'ı decode edememesidir,
sunucu duvarı değil. Fan-out duvarını kaydıran **mesafe-yanlı
görünürlük**tür (§8.1), segmentasyonla *birleşir*. (4) **Yeni duvar
konumu:** 10k (oda cap) sınırında adım-duvarı aşılmıyor; bir sonraki
duvar oda cap'i / bağlantı-altyapısı, adım değil. Segmentasyon tek
sürekli dünya sınıfına aittir; bölünebilir oyunlarda çoklu oda daha
ucuz aynı işi yapar.

**Kapsam notu / yapılmayanlar.** Shard'lar **tek makine** içindeki
actor'lardır (çok makine dağıtımı — shard'ların network'e yayılması —
yapılmadı, §14.4). Shard sayısı 1..=16 (grid sınırlı). Sınır
entity'si en fazla 1 tick geri görünür (lag-blink: ≤1 tick yok, asla
kopyalanmaz) — kabul edilen dejenerasyon. Migrasyon sayaçları
`RoomSample`/`RoomReport`'a **eklenmedi** (kapsam kontrolü; yalnız
`tracing` logları).

