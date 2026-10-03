# gsb Mimari Tasarım Dokümanı

Bu doküman, gsb'nin tasarım kararlarını, ölçekleme hedeflerini, bilinçli
kısıtlarını ve yol haritasını içerir. Kod İngilizce yorumlanır; bu doküman
ve sohbet Türkçe'dir.

## 1. Amaç ve kapsam

**Amaç:** MOBA/MMORPG sınıfı, çok oyunculu, gerçek zamanlı oyunlar için
100k+ eşzamanlı bağlantıyı hedefleyen bir sunucu temeli.

**Kapsam dışı (v1):** oyun mantığı (demo hariç), kompresyon, kalıcılık,
cross-server (cluster), yük dengeleyici. (AOI/görünürlük stratejileri
önceki turlarda, `spatial` stratejisinin delta yayını bu turda kapsam
içine alındı — §8.1.)

## 2. Temel ilke: saf kanal tabanlı aktör model

Tek kural: **multiplex yok — her görevin tek bir beklenecek kaynağı vardır.**
Aktörler mailbox'ına gelen mesajı bekler; oda actor'leri global tick
broadcast kanalındaki tek bir tick'i bekler; pump görevleri birer stream
öğesi/kanal mesajı bekler. Birkaç görev (ör. connection actor'ün join'inde)
oneshot yanıtını da bekler — yine de tek bekleme, select değil.

Bu kural üç somut biçimde uygulanır:

1. **Hiçbir `tokio::select!` yok.** Çoklu bekleme ihtiyacı, beklenecek her
   kaynak için **ayrı bir görev** açılarak çözülür (pump görevleri, global
   ticker görevi, bağlantı dispatcher'ları). "Birden fazla kaynağı tek
   görevde multiplex etme" deseni bu mimaride var olmaktan çıkar; bu, büyük
   sistemlerde bug'ların ana kaynağıdır.
2. **Hiçbir kilit yok.** Durum, ait olduğu aktörün (veya görevin) yerel
   değişkenlerinde yaşar. Aktörler arası her değer (mailbox, oneshot yanıt,
   frame batch) kanallarla *taşıma* (move) edilir; paylaşım yoktur.
3. **Kural derleme zamanında denetlenir.** `gsb-lint` lint kullanımı olan
   her crate'in `build.rs`'inde `src/`, `tests/` ve `examples/` ağaçlarını
   tarar; `tokio::select` (veya çıplak `select!`), `futures::select`,
   `Mutex`/`RwLock` (çıplak alt dize — `use std::sync::Mutex;` formu dahil),
   `parking_lot` kalıplarıyla karşılaşırsa derleme **hata** ile biter.
   Yorum/doküman metni sayılmaz (önce yorumlar soyulur, satır numaraları
   korunur). Lint, tarama kapsamındaki her dosya için
   `cargo:rerun-if-changed` yayınlar; yoksa build script'teki başka
   direktifler (ör. prost'un proto dosyası) cargo'nun yeniden çalışma
   davranışını daraltıp taramayı sessizce devre dışı bırakırdı.

Neden? Performans gerekçesiyle `tokio::select!`'ten kaçınmak istedik:
select tabanlı aktörler, (a) her mesajda tüm kaynakları yeniden poll etmek,
(b) poll sırasına bağlı belirsiz davranış ve (c) waker yönetimi hatası
alanı yaratır. Kanal + görev deseni: sabit O(1) bekleme, sırayla işleme,
ve "kaynak yok" hali imkânsız (kanal kapanmasıyla net bir son vardır).

## 3. Görev topolojisi

```text
                 ┌────────────────────────────────────────────────────────┐
                 │                        registry actor                  │
                 │  (oda tablosu + bağlantı tablosu + RoomFactory)        │
                 └───────▲───────────────▲───────────────▲────────────────┘
             ConnOpened/ │               │               │  SpawnPlayer/
             ConnClosed  │               │               │  DespawnPlayer
        ┌────────────────┘               │               └────────────────┐
        ▼                                ▼                                ▼
 accept loop                    connection actor                 room actor (×N)
 (bağlantı başına)              (bağlantı başına)                (oda başına)
        │                                │   ▲                          ▲   ▲
        ▼                                │   │                          │   │
   ┌─────────┐                          │   │ mailbox(ConnIn)          │   │
   │ endpoint│  reader pump ────────────┘   └───────────────────────────┘   │
   └─────────┘                                                                │
        ▲ writer pump ◀── out: mpsc<FrameBatch> ◀────────────────────────────┘
        │                                                            (fan-out)
        ▼
      socket

  global ticker (tek görev) ── broadcast<TickInfo> ──▶ her room actor'un tek await'i
                                          │
                                          └────────────▶ metrik toplayıcı görevi (tek await'i
                                                         aynı broadcast; rapor süresi ≥ 1 s;
                                                         bkz. §12)

  registry / room / connection actor'ler ── MetricsEvent örnekleri ──▶ metrik toplayıcı
                                          (mpsc::unbounded, senkron gönderim —
                                           oda tick'ine await EKLEMEZ; bkz. §12)
```

- **Bağlantı başına 3 görev:** reader pump (socket → `ConnIn::Frame`),
  connection actor (durum makinesi: auth → join → forward), writer pump
  (`FrameBatch` → socket). Pump görevlerinin her biri de tek kaynaktan
  bekler: stream'in bir öğesi ya da kanal mesajı.
- **Oturum saati = reader pump'un read deadline'ı.** Reader pump her
  `stream.next()`'i `tokio::time::timeout(idle_timeout)` ile sarmalar
  (konfig: `idle_timeout_secs`, varsayılan 30 sn; `0` = kapalı). Pencere
  içinde istemci frame'i gelmezse pump `ConnIn::ServerClosed { reason }`
  yollar; connection actor ERROR kod 9 gönderip cleanup kaskadından çıkar
  (bkz. §5, §9). **Her** frame (HEARTBEAT dahil) pencereyi sıfırlar. Bu,
  bağlantı yolundaki *tek* saattir: connection actor'ün tek await'i inbox
  `recv`'de kalır (spec'in sert şartı; `select!` yok), bağlantı başına
  timer görevi / registry mesajı / ticker aboneliği eklenmez. Yarım açık
  TCP (kablo çekildi, güç kesildi — FIN/RST gelmez) "socket durumu" ile
  değil, "T süre boyunca frame yok" ile yakalanır. 100k bağlantıda maliyet:
  0 ekstra görev, ~15 MB bekleyen `Sleep` (~150 B × 100k), ~1–2 core
  (yalnızca gerçekten boşta olan bağlantılar uyanır). Elenen
  alternatiflerin 100k matematiği: CHANGELOG "Kapatılanlar (koruma katmanı
  turu)".
- **Geç ateşlenen son tarih = sürecin takılması (F72).** Pencere duvar
  saatidir; süreç pencereden uzun takılırsa (swap, VM duraklaması, aç
  kalmış runtime) uyanışta her canlı oturumun son tarihi geçmiş olur ve
  hepsi `idle_timeout` kapanırdı (t1: loadgen'i 2 sn'lik pencereye karşı
  3 sn dondurmak `left=0 server_close_idle_timeout=4`). Son tarih NEDEN
  hiçbir şey gelmediğini bilemez ama GEÇ ateşlendiğini bilir: hedefinden
  `IDLE_STALL_GRACE` (250 ms) fazla sonra ateşlenen son tarih sürecin
  takılmasıdır, pencere yeniden başlar (sayılır:
  `idle_windows_restarted_late`, OPS §3). Sessizlik başına BİR kez:
  yeniden başlayan pencere de sessiz biterse oturum — o ateşleme ne kadar
  geç olursa olsun — kapanır; kalıcı aç kalmış bir süreç yarım-açık
  oturumu sonsuza dek tutamaz (sınır: iki pencere + takılmalar). Kare
  hakkı yeniler. Eşik sabit, pencerenin kesri değil: SÜRECİ yargılar.
  Ölçüm (bu makine, 500 tokio zamanlayıcısı, 1 sn'lik uykular): sessizde
  en geç 18 ms, iki çekirdeğe sabitlenip 24 `yes` yanında 12 ms; aynı yük
  altında nice 19 ile (yavaşlamış değil aç kalmış süreç) p50 17 ms, p99
  1,3 sn. 250 ms ikisinin arasında ve `CUT_GRACE`'in "sağlıklı görev bu
  kadar sürmez" sınırıyla aynı. **Hükümden önce bir bakış (F34):**
  zamanlayıcı ile soketin hazır olması ayrı sürücü olaylarıdır; sokette
  bekleyen kare pump'ı uyandıran zamanlayıcıdan sonra raporlanabilir.
  Hükümden önce pump bir kez `yield` eder (runtime, bırakılan görevi
  yeniden çalıştırmadan IO sürücüsünü yoklar) ve akışı beklemeden bir kez
  daha okur: kare oradaysa kazanır. rUDP süpürmesi aynı kuralı alan
  eklemeden uygular: oturumun ilk pencere girdisi tam `last_seen + idle`,
  yeniden başlatılanınki daha geç (uyanış + idle), daha erkeni bayattır.
  rUDP'de hükümden önce bakış YOK: demux'ın her döngüsü son tarihten önce
  bekleyen datagram'ı okur (sıfır süreli `timeout` önce okumayı yoklar);
  bütçe (`coop`) tükenirse süpürme gelir ama o zaman son tarih genelde
  geç ateşlenmiştir. Kapsam dışı pencereler: QUIC'in protokol
  `max_idle_timeout`'u (quinn'in kendi zamanlayıcısı, `IDLE_TIMEOUT` 30 sn
  — değiştirilemez), rUDP REL bandının canlılık sınırı
  (`REL_NO_ACK_FATAL`, yazıcı), odanın girdi-boşta tavanı (tick saati),
  park süresi, el sıkışma son tarihleri (BACKLOG c2 satırları).
  Ölçüm (t1'in düzeneği: süreç içi loadgen, 4 istemci, 2 sn pencere,
  2,5 sn'de 3 sn SIGSTOP): rUDP önce 3/3 `left=0
  server_close_idle_timeout=4`, sonra 3/3 `left=4`, 0 kapanış (2 koşuda
  4 yeniden başlatma, birinde datagram'lar süpürmeden önce okundu: 0);
  TCP iki kural da kapalıyken 3/3 4 kapanış, yalnız bakış açıkken 3'te 1
  kapanış, ikisi açıkken 3/3 0 kapanış (3–4 yeniden başlatma).
- **Oda başına 1 görev:** room actor. Tick'ler **tek global ticker görevinden**
  gelir (`tokio::sync::broadcast`): oda actor'ünün *tek* await'i
  `tick_rx.recv()`; tick gövdesi tamamen senkron. Oda hizi global hızın tam
  bölünürü olmalı — 15 Hz oda, 60 Hz saatte her 4. tick'te adım atar
  (`run_every`).
- **Registry:** sunucunun kontrol düzlemi. Oda tablosu
  (`RoomId → kontrol mailbox'ı`), bağlantı tablosu
  (`ConnectionId → {room?, entity?, inbox?}` — kayıt, bağlantının **bütün
  ömründe** yaşar; inbox asla bırakılmaz ki `RoomGone`/`Shutdown` her zaman
  ulaşabilsin) ve oyun mantığını core'e sokan `RoomFactory<W, G>` kapağı
   (`G` = oyun mantığının snapshot grup anahtarı, bkz. §4/§8).
  Registry **asla bir odayı await etmez**: join/leave odasıyla olan
  gidip-gelişler, bağlantı başına küçük **dispatcher görevlerine**
  devredilir. Dispatcher, o bağlantının oda mesajlarının **tek** göndericisi
  olduğundan join→leave→rejoin sırası garanti (stale leave, yeniden
  giren entity'yi asla öldüremez). Tek yavaş oda kontrol düzlemini asla
  bloke edemez. Bağlantı kapanınca registry dağıtıcıya `Close` yollar ve
  göndericiyi bırakır; dağıtıcı kuyruğunun kapanmasını da `Close` sayar
  (önündeki op'lardan sonra elindeki üyeliği DETACH eder), dolu kuyruğun
  reddettiği `Close` üyeliği sızdırmaz. Görev gitmişse registry tablodaki
  üyeliği doğrudan (spawn'lu) DETACH eder (B61, RECONNECT §3.4).
- **Accept loop:** `ConnectionId` üretir, pump görevlerini başlatır,
  `ConnOpened`'ı **actor'ü başlatmadan önce** registry'e gönderir (ilk
  istemci frame'ine karşı sıralama garantisi). Kalıcı hata durumunda
  (ör. `EMFILE`) 100ms backoff ile dener — CPU spin'i olmaz.
- **Başlangıç odaları her join'in önünde (B44).** `room_count`
  odalarının `CreateRoom`'ları, başlatma prosedüründe accept döngüleri
  spawn edilmeden ÖNCE, id sırasıyla registry mailbox'ına satır içinde
  (`try_send`) konur (`boot/start/boot_rooms.rs`). Registry mailbox'ını
  FIFO boşaltır ve bir `CreateRoom` odayı tablosuna bir sonraki mesajdan
  önce koyar (fabrika registry döngüsünde koşar); accept döngüsünden
  gelen her `ConnOpened`/join daha sonraki bir mesajdır, yani oda
  vardır. Yalnız CEVAPLAR spawn'lı bir görevde beklenir: başlatma hiçbir
  odayı beklemez, bir oda başlatmayı (ve `start`'ın döndürdüğü tutamağa
  muhtaç `stop`'u) asamaz. Eskiden her `CreateRoom` spawn'lı bir
  görevden gidiyordu; yükte o görev accept'ten sonra koşunca hemen
  katılan istemci `room 1 not found` alabiliyordu (~600 loadgen
  koşusunda 3 kez). Kalan tek durum: mailbox'ın boş kapasitesini (4096)
  aşan başlangıç odaları — onlar beklemeli gönderim ister, o yüzden
  spawn'lı görevden (yine sıralı) gider ve başlatma bunu `warn` ile
  söyler. *Elenen:* oluşturmaları accept'ten önce beklemek — doğruluk
  için gereken sıra, tamamlanma değil; beklemek başlatmayı her odanın
  fabrikasına bağlar. Kilit: `tests/boot_rooms.rs` (current-thread
  runtime'da `start` döner dönmez `room_status` — spawn'lı görev henüz
  koşmamışken — `Running` görmeli) ve `boot_rooms::tests` (sıra, taşma).
- **Metrik toplayıcı:** odaların/registry'nin/bağlantıların sayacalarını
  **kanaldan** toplayan tek görev (bkz. §12). Saat kaynağı ticker'ın
  broadcast'i — odalarla aynı tek-await disipline sahiptir; ticker kapanınca
  oturum üreticilerinin (odalar, bağlantılar) son sözlerini bekler (en çok
  `FINAL_REPORT_GRACE`), son raporu basıp çıkar (F35, §12).

## 4. Oda tick'i: 5 faz

Room actor'ün tek `await`'i global tick broadcast kanalındaki `recv()`; tick
gövdesi tamamen **senkron**'dur:

```text
Global ticker ── broadcast<TickInfo{tick, at}> ──▶
  (tick % run_every != 0 ise atla — yavaş odalar için)
  0. CONTROL:   join/leave/shutdown (kontrol kanalı, try_recv)
  1. READ:      bağlantı başına aksiyon kanalları (try_recv, bloksuz)
  2. CONVERT:   aksiyon → component yazıları  (RoomLogic::ingest)
  3. SYSTEMS:   sıralı oyun sistemleri        (RoomLogic::update)
  4. BROADCAST: grup başına tam snapshot — BİR KEZ kodla, freeze(),
                referansla dağıt + bağlantı başına private frame
                (RoomLogic::snapshot / ::group_of / ::private)
```

- **Kare hızından bağımsızlık:** `dt = son adımdan bu yana geçen duvar
  saati` (tick'in `at` zaman damgası). Bir tick kaçırılırsa (yavaş adım,
  buffer'ı zorlayan yük) geçen süre sonraki adımın `dt`'sine zaten dahil —
  simülasyon gerçek zamanın gerisine düşmez; 15 Hz'de de 100 Hz'de de aynı
  gerçek sürede aynı mesafe kat edilir. `dt` üst sınırı `max_catchup`
  periyottur (varsayılan 4): uzun bir takılmadan sonra simülasyon kısa bir
  "yavaş çekim"le saate döner, sıçramaz.
- **`Lagged`:** receiver broadcast buffer'ının gerisinde kalırsa aradaki
  tick'ler atlanır (uyarı kaydı); bir sonraki adımın duvar saati `dt`'si
  boşluğu kapsar. **`Closed`:** ticker durduruldu = global stop sinyali →
  oda temiz çıkar.
- **Kontrol tick sınırında:** join/leave/shutdown, adımın başındaki CONTROL
  fazında işlenir; join/leave gecikmesi ≤ 1 tick. Bu bir maliyet değil,
  **determinizm garantisi**'dir: spawn/leave bilinen bir tick'te etkide
  bulunur; stale leave'ler ayrıca entity eşleştirilmesiyle korunur.
- **Girdi izolasyonu (sınırlı çekme / bounded pull):** her bağlantının
  kendi `Action` kanalı var; READ fazı `try_recv` ile bloksuz çeker — ama
  **bütçeyle**, eski sürümdeki gibi merge edilmiş tek listeye değil:
  bağlantı başına tick başına `max_actions_per_conn_per_tick` (varsayılan
  16 = 30 Hz'de ~480 aksiyon/sn) ve oda başına tick başına
  `max_pending_actions` (varsayılan 65536). Oda **çekişini yaptığı
  aksiyonu asla atmaz** — bu yüzden oda kapsamında bir girdi-düşme sayacı
  **yoktur**: olsaydı yalnızca 0 raporlayabilirdi ve operatör bunu "girdi
  hiç düşmüyor" diye okurdu (bkz. "teknik borç turu", madde a); bağlantı o tick'te odaya
  ne gömebileceği **kendi** bütçesiyle sınırlıdır, dolayısıyla bir
  flooding bağlantı başka bir bağlantının aksiyonunu **artık evicted
  edemez** (eski merged-list'in "aşım → en eskiyi at" davranışı,
  saldırganın backlog'u başkalarının aksiyonlarını atıyordu; kimin
  atılacağı hash sırasına bağlıydı). Tek kayıp noktası gönderici tarafında:
  bağlantının kendi `Action` kanalı doluyken `try_send` Full — connection
  actor bu düşmeyi **kendi** metrik örneğinde sayar
  (`actions_dropped`, raporda `actions_dropped_top` ile bağlantıya
  atfeli; §12). Oda belleği sınırlı kalır, hasar saldırganın kendi
  girdisiyle sınırlıdır; bağlantı ne hata alır ne kopar (koparmak
  reconnect/backoff fırtınasıyla saldırganı *amplify* ederdi).
- **Girdi HACMİ (opt-in, BACKLOG E1; SECURITY §3.4):** per-tick çekme
  bütçesi odayı korur, göndericinin hızını değil — sınırın ötesi
  kanalda birikir. Bir oyun (ya da operatör) saniye başına hacmi de
  sınırlamak isterse `RoomConfig::input_rate` (`InputRate { per_sec,
  burst }`, varsayılan `None` = kapalı) bağlantı başına bir **token
  bucket** açar. Uygulandığı yer oda DEĞİL, bağlantı aktörüdür: protokol
  kontrollerinden (tanımsız opcode, odada değil) sonra, `try_send`'den
  önce — sınırın üstündeki girdi kanala hiç girmez, oda onu ne çeker ne
  atar; bu yüzden "oda çektiğini asla atmaz" ve "tek kayıp noktası
  gönderici tarafında" ilkeleri aynen durur, yalnız gönderici tarafında
  ikinci ve bilinçli bir sayaç eklenir (`input_rate_limited`, ihlal
  değil; `actions_dropped`'tan ayrı: biri kanal doldu, öbürü oyunun
  sayısı aşıldı). Sayı odanın: registry join'de odanın config'inden
  damgalar, `Seat` (entity + aksiyon kanalı + hız) ile bağlantıya verir;
  kova bağlantınındır ve oda geçişinde yeniden ayarlanır, dolmaz. RPC
  istekleri ve kontrol bandı ölçülmez. Kova O(1), tahsissiz,
  zamanlayıcısız: seviye varışta tick saatinden (`ticker::now()`)
  hesaplanır. İki bütçe birbirinin yerine geçmez: çekme bütçesi girenler
  arasında adalettir (fazlası bekler), hız sınırı ne gireceğidir (fazlası
  düşer).
- **BROADCAST fazı (grup başına tam snapshot):** Bağlantılar oyun
  mantığının `RoomLogic::group_of()` ile **snapshot gruplarına** ayrılır
  (`GroupKey`: `Eq + Hash + Clone + Debug`; demo'da `()` = oda başına tek
  grup, `ConnectionId` = bağlantı başına grup — arayüz ikisini de taşır).
  Her tick
  oda her grup için snapshot'ı **bir kez** `RoomLogic::snapshot()` ile
  kodlar, `freeze()`'ler ve üyeleriyle `Bytes` (Arc refcount) klonu olarak
  paylaşır — payload **asla** bağlantı başına kopyalanmaz, bağlantı başına
  kodlama yoktur (eski `OutSink` + `last_sent` yapısı kaldırıldı). Oda
  grup başına durumu (son gönderilen snapshot + bu tick'in gönderimi +
  tanı bayrağı) bir `HashMap`'de tutar; bu yüzden oda actor'ü
  `RoomActor<W, G>`'dir ve registry/factory `G` üzerinden geniktir.
  - **"Değişiklik yok"** kararını oyun mantığı verir (`snapshot` →
    `false`); tanım **üyelik değişimini (join/leave) da** kapsar. Hiçbir
    grup değişmediyse oda o tick'te **hiçbir şey göndermez** — tek istisna
    **keepalive**: her `tick_hz / keepalive_hz` (varsayılan 1 Hz) odasal
    adımda değişmeyen her grup, son önbellekli snapshot'ını yeniden
    gönderir (yeniden kodlama yok, önbellek klonu). Aksi hâlde son
    paketini kaybeden istemci kalıcı olarak bayat kalırdı.
  - **"Değişiklik yok" defteri grup başına tutulmalıdır.** Oda tick
    başına her mevcut grup için `snapshot()`'ı **belirsiz sırada** birer
    kez çağırır (group tablosu bir `HashMap`'dir; sıra çalıştırma içi
    sabittir ama güvenilmemelidir). Mantığın "son gönderim" defteri
    `group` anahtarıyla tutulmalı ve bir çağrı aynı tick'te başka bir
    grubun cevabını değiştirmemelidir. Tek paylaşımlı defter yalnız
    `GroupKey = ()` (tek grup) odalarda doğrudur — demo'nun `last`
    alanı tam olarak budur. Birden çok grupta önce ziyaret edilen grup
    değişikliği tüketip defteri yazar, sonraki gruplar koşunun geri
    kalanında "değişiklik yok" görür: üyeleri **aç kalır** (keepalive bile
    bayat önbelleği yeniden gönderir; önbellek hiç dolmadıysa hiçbiri
    gönderilmez) ve oda bunu **teşhis edemez** — sessizlik, gerçekten
    değişmeyen bir grubun meşru hâlidir. Gerçek olay: `GroupKey =
    ConnectionId`'a birebir kopyalanmış demo mantığında 2 bağlantı,
    her tick hareket eden 1 entity, 25 tick'te dağılım 1/26 (kaybeden
    grup, kendi join tick'inden beri yeni snapshot alamadı).
  - **Tanı:** üyesi varken hâlâ hiç snapshot üretmemiş grup — ilk
    tick'te `snapshot` → `false`, oysa yeni bir grubun ilk tick'i
    üyelik değişikliğidir ve zorunlu yayındır; yani sözleşme ihlali —
    oda tarafından **bir kez** `warn!` ile loglanır (grup adı + üye
    sayısı; `GroupKey`'ye `Debug` bound'u bu içindir; test:
    `never_emitted_group_warns_once_naming_the_group`). Kapsam
    bilinçli olarak sınırlıdır: yalnızca *sessiz* (en az bir kez
    yayınlamış) gruplar bu uyarıyı tetikleyemez ve iki somut durum
    yakalanamaz. (i) Paylaşımlı defterle aç kalan bir grup kendi join
    tick'inde yayın yapmışsa (son snapshot'ı o tick'tendi) `last` dolu
    görünür ve uyarı **asla tetiklenmez** — gerçek 1/26 aç kalma
    ölçümü tam bu moddaydı ve ölçümde uyarı çıkmadı (denetim turu
    probe'u: 16 (A,B) çifti, her iki ziyaret sırası oluştu; bu moddaki
    tüm çiftlerde uyarı 0 — ham çıktı raporda). Bu "oda tarafından
    yapısal olarak tespit edilemez" hükmü, 2aad7ea üstü denetim
    turunda odanın her tick'te elindeki bilgi tek tek sayılarak yeniden
    sınandı ve **doğrulandı**. Oda bir grup için yalnızca şunları
    görür: (a) grup tablosundaki `GroupState` (son yayınlanan
    snapshot baytları, bu tick'in gönderimi, tanı bayrakları — tümü
    geçmiş gözlemlerin türevi), (b) üyelik kümesi (bağlantı
    tablosundan her tick yeniden kurulan), (c) önceki tick'in durumu
    (`GroupState` tick'ler arası yaşar), (d) `snapshot()`'ın dönüş
    değeri (`bool`; `true` ise payload baytları). Aç kalan grup ile
    **son yayınlanan snapshot'ından sonra içeriği gerçekten donan**
    meşru grup — AOI'de grubun alanındaki tek hareketli entity o
    andan sonra alanı terk etmiş ya da hareket durmuş; demo'da bu,
    tek oyuncunun hedefine ulaşmasıyla gerçekleşen sıradan bir
    senaryodur — odanın (a)–(d) gözlem serisine **bire bir aynı**
    düşer: aynı üyelik, aynı join-tick payload'ı, ardından sonsuza
    kadar `false`. Bu gözlem serisini alan her deterministik oda içi
    test, ihlalde uyarı veriyorsa meşru senaryoda da yanlış alarm
    verir; meşru senaryoda susuyorsa ihlali de göremez. Oda daha
    derine bakamaz: payload gsb-core için opak bayttır (core oyun
    mesajlarını decode etmez — core/oyun sınırı; örneğin `sequence`'ı
    core'da okumak hem bu sınırı bozar hem de yetmez: bu modda yeni
    payload hiç yayınlanmadığı için `sequence` de ilerlemez), dünya
    `W` opak'tır, teslim onayı (ack) mekanizması yoktur, keepalive
    yeniden gönderimi `last`'i klonlar (yeni bilgi taşımaz), üyelik de
    join tick'inden beri sabittir. Dolayısıyla bu mod yalnızca
    sözleşme metniyle (grup başına defter şartı, yukarıdaki defter
    maddesi) korunur. (ii) tek tick ihlal eden grup (join + leave
    aynı tick'te) `gone` budamasıyla uyarıdan önce tablodan düşer.
  - Bağlantı başına teslim: tek batch = [grubun snapshot'u (paylaşımlı
    `Bytes`)] + [private frame, eğer `RoomLogic::private` ürettiyse];
    tek `try_send`. Kanal doluysa batch atılır ve sayılır
    (`dropped_frames`); snapshot'lar kendi kendine yettiği için bu yalnızca
    o istemciye 1 snapshot bayatlık olarak yansır (keepalive sınırlar).
    **Düşme sinyali (F11):** atılan batch mantığa bildirilir —
    `GameLogic::on_batch_dropped(world, player, snapshot)`, AYNI fan-out
    yinelemesinde, o oyuncunun `private` çağrısının hemen ardından (başka
    oyuncunun `private`'ından önce); `snapshot` = batch'te grubun karesi
    vardı. Çekirdek batch'i yeniden denemez, kanalı beklemez (fan-out
    best-effort ve senkron kalır, yeni kanal yok); "gönderdim" diye tek
    seferlik durum türeten mantık (bir baseline, bir karşılama, bir ack)
    onu burada yeniden kurar ve sonraki bir batch'le yollar — o batch de
    aynı düşmeye tabidir, fırtına sınırı mantığındır. İkinci kanca
    `GameLogic::on_batch_resumed(world, player)`: bir düşme dizisinden
    sonra kanalın kabul ettiği İLK batch, aynı noktada (satırda tek
    `bool`, `RoomConn.dropping`; başarılı gönderimde tek dal; taze
    taşıma — join, resume, göç varışı — temiz başlar). Mantık, dolu
    kanala karşı temposunu burada bırakır: bekleyen yeniden gönderim bir
    sonraki kareye biner, geri çekilmenin sonunu beklemez. Varsayılan
    no-op (ikisi de): sinyali okumayan mantık bugünkü gibi davranır.
    Detached satırlar hiç göndermediği için bildirilmez. Kapalı kanal
    (`Closed`) da `dropped_frames` gibi sayılır ve bildirilir (batch
    teslim edilmedi). Kit'in cevabı: KIT-ARCHITECTURE §10 "F11".
    **Çekirdeğin kendi yükü — RPC yanıtları (F14):** batch'te çekirdeğe
    ait tek içerik, `private`'a verilen `responses`'tır. Düşmede bunlar
    bağlantının `queued` kuyruğunun başına geri konur (fan-out
    `queued`'ı süpürdükten sonra; yerel bir `Vec`, sessiz yolda ayırma
    yok) ve kanalın kabul ettiği ilk batch'e kadar her tick `private`'a
    yeniden, sonraki yanıtlardan önce verilir — tam bir kez, sırayla.
    Fırtına sınırı: tıkalı bağlantı (`dropping`) borcu — kuyruk +
    taşınan + uçuştaki — `max_pending_requests_per_conn`'a ulaşınca yeni
    isteği işlemeden ve yanıtlamadan reddeder; borç en çok cap + bir
    tick'in çekim bütçesidir (varsayılan 4 + 16). Ayrılış/detach/göç
    teslim edilmemiş yanıtları oturumla birlikte götürür. Ayrıntı:
    RPC-CONTROL-PLANE §3.1.
    Batch buffer'u bağlantı başına kalıcıdır (`RoomConn.batch`): tick
    başına `clear()` + `mem::take` ile kanala teslim — ısınma sonrası
    tick başına bağlantı başına sıfır heap tahsisi (ölçülen taban
    dilimlerinden biriydi: eski `Vec::with_capacity(2)` × N bağlantı;
    kanal doluysa buffer `into_inner` ile geri konur — ROADMAP "ölçüm
    çözünürlüğü + taban turu").
  - Snapshot payload'u `RoomConfig::max_snapshot_bytes`'i aşarsa uyarı
    loglanır (~~rUDP'de MTU hazırlığı: aşırı snapshot datagram'a sığmaz~~
    *(kapandı — U turundan beri rUDP bütçeyi aşan oyun bandı karesini
    taşımada parçalar, §6 "MTU"; uyarı artık bant sinyali)*; sürekli
    uyarı = grubu bölme/AOI zamanı, §8).
  - Kodlama maliyeti O(grubun entity sayısı)/grup/tick; fan-out O(üye)
    Arc klonu + `try_send`. 800 oyunculu oda, tek hareket eden entity:
    adım maliyeti ~65 ms → ~0.14 ms (release, bkz. §11 altındaki tablo).


## 5. Ağ protokolü

```text
[u32 LE gövde uzunluğu][u16 LE opcode][protobuf payload]
```

- 4 byte uzunluk öneki **transport'un işidir** (TCP'de
  `LengthDelimitedCodec`, `max_frame_length` korumalı). Aktör katmanı yalnızca
  `[u16 opcode][payload]` gövdesi görür; böylece rUDP'de "uzunluk" kavramı
  farklı anlam kazanabilir, üst katman bilmez.
- **Opcode bantları:** `1..=64` temel kontrol (AUTH_REQ, AUTH_RESULT,
  JOIN_ROOM_REQ, JOIN_ROOM_RESULT, LEAVE_ROOM_REQ, LEAVE_ROOM_RESULT,
  HEARTBEAT, HEARTBEAT_ACK, ERROR; `10`/`11` = UDP_HELLO/UDP_ACK —
  rUDP **taşıma işaretleri**, aktör katmanının altında işlenir, mesaj
  tablosunda değiller; `13` = UDP_REPORT, aynı türden işaret — istemcinin
  oyun bandı raporu demux'tan yazıcıya bu opcode'la geçer, §6 "Oyun bandı
  geri bildirimi"; `14` = UDP_PATH, aynı türden ve hiç tele çıkmaz —
  demux'ın "oturum doğrulanmış yeni adrese göçtü" bildirimi yazıcıya bu
  opcode'la gider, §6 "Bağlantı göçü"; `16` = UDP_SEND, aynı türden ve
  opcode olarak hiç tele çıkmaz — mühürlü kapıda demux'ın kendi
  datagramını (ACK, PATH_CHALLENGE) mühürleyip göndermesi için yazıcıya
  isteği, §6 "Kayıt katmanı"); `1000+` oyun bandı
  (MOVE_TO=1000, WORLD_SNAPSHOT=1003; PRIVATE=1004 `RoomLogic::private`
  için ayrılmış, demo kullanmaz; 1001/1002 boş — eski ENTITY_SPAWNED /
  ENTITY_REMOVED kaldırıldı, üyelik snapshot'ta var olmaya indirgendi).
- **ERROR kodları** — artık bir yorum tablosu değil, gerçek bir proto
  `enum`: `gsb.base.ErrorCode` (bkz. §5.4). `base.Error.code` alanının
  tipi `uint32`'den `ErrorCode`'a geçti; tel değişmedi (proto3 enum'ı da
  varint, 1..12 aralığında bayt-birebir aynı). Numaralar ve anlamları
  değişmedi: `1` bilinmeyen opcode, `2` decode hatası, `3` auth (authsuz
  / tekrar auth), `4` oda işlemi başarısız (oda yok / tick hızı
  uyuşmazlığı — istemci kararı "geçici/bilinmiyor, tekrar denenebilir"),
  `5` oda imha edildi, `6` odada değil, `7` diğer, **`8` oda dolu**
  (nazik reddi — bağlantı **yaşar**, başka odaya join edebilir; sessiz
  kapatma reconnect fırtınası üretirdi), **`9` sunucu kapattı** — sunucunun
  BU oturum hakkındaki hükmü (idle timeout, sunucu bağlantı cap'i,
  protokol-ihlal ya da pre-auth kare bütçesinin tükenmesi, aynı oyuncunun
  yeni oturumu, taşımanın bayt akışını reddetmesi — `stream rejected: …`;
  mesaj hangisi olduğunu söyler; hemen ardından bağlantı kapatılır), `10`
  bilet doğrulama başarısız, `11` join bileti eşleşmiyor, `12` oda emekli,
  `13` protokol sürümü uyuşmazlığı (§5.5), **`14` sunucu duruyor**
  (`ServerHandle::stop`; oturum hakkında hüküm DEĞİL — sonra ya da başka
  sunucuya bağlan; ardından kapatma; §5.6).
- **Protokol sürümü:** `Auth.protocol_version` (alan 3) — oturum başına
  tek kontrol, AUTH'ta. `0` = sürümsüz/legacy (kabul, uyarı loglanır),
  `gsb_protocol::PROTOCOL_VERSION` = kabul, başka her şey = ERROR 13,
  bağlantı yaşar. Ayrıntı ve elenen alternatifler: §5.5.
- **Seriştirme:** protobuf. Rust tarafında `prost`, Unity tarafında
  `Google.Protobuf` — aynı `.proto` dosyaları her iki tarafta kullanılır.
  Mesajlar `MessageTable`'da opcode→(de)koducu olarak kayıt edilir; tablo
  başlangıçta bir kez kurulur ve `Arc` ile salt okunur paylaşılır (hiçbir
  yazma, hiçbir kilit).
- `FrameBody { op, payload: Bytes }` — `Bytes` sayesinde payload kopyasız
  akar (socket → actor → oda → yayın, tek kopya).
- **rUDP'nin tek bilinçli uyumluluk kırılması (B5a, karar 6 —
  güvenlik):** mühürlü kapı (sunucunun varsayılanı) düz metin istemcinin
  proof'unu reddeder (`udp_proofs_refused_plaintext`; istemci
  `TimedOut`), mühürlü istemci düz metin kapının kabulünü reddeder
  (`ConnectionRefused`). rUDP hattının geri kalan her eklemesi (FRAG,
  PROBE/REPORT, CID) eski uçla konuşmaya devam etti; bu etmez, çünkü
  "eski istemciyle düz metin konuş" bir saldırganın da seçebileceği bir
  geri düşüş olurdu. Düz metin isteyen dev/LAN kapısı bunu açıkça söyler
  (`udp_security = "plaintext"`). Matris ve tel: §6 "Kayıt katmanı".

### 5.1 Band seçimi: kaybın bedeli, boyut değil

Yeni bir opcode eklerken güvenilir (≤64) mi kayıp-toleranslı (≥1000)
banta gideceğine karar kriteri **boyut değil, kaybın bedelidir**:

| "Bu mesaj kaybolursa..." | Bant | Örnek |
|---|---|---|
| ...istemci sonsuza kadar bekler / durum kalıcı bozulur | Güvenilir (≤64) | JOIN_RESULT, AUTH |
| ...sonraki mesaj onu geçersiz kılar / kendini onarır | Kayıp-toleranslı (≥1000) | WORLD_SNAPSHOT, MOVE_TO |
| ...onay gerektiren ayrık bir işlemdir | RPC deseni (`RPC_REQ`, güvenilir yolda) | satın alma, takas |

Boyut kriter değil, **kısıttır**: datagram/RAW yolu ~1350 B güvenli
yükle sınırlıdır (~~sığmayan atılır+sayılır~~ *(U turundan beri rUDP
oyun bandında sığmayan kare FRAG ile parçalanır — mesaj başına en çok
16 parça; yalnız onu aşan atılır+sayılır, kontrol bandı parçalanmaz —
§6 "MTU")*; telafi sonraki tam snapshot'tır); stream yolu kendisi
parçalar. Kural transport-bağımsızdır:
opcode bandı taşımayı belirler (rUDP REL/RAW ve QUIC stream/datagram
aynı tabloyu uygular). Oyun geliştiricinin pratik kuralı: garanti
gerektiren oyun işlemi için yeni taşıma icat etme — RPC desenini kullan
(bkz. `docs/RPC-CONTROL-PLANE.md`).

### 5.2 Mesaj sahipliği: hangi `.proto` dosyasına ne girer

Kural: **bir mesajın sözleşmesini core belirliyorsa mesaj
`base.proto`'dadır; oyun belirliyorsa `game.proto`'dadır.** Sahiplik
mesajın nereye *gömüldüğüne* değil, kuralını kimin yazdığına bakar.

Bu ayrım protokol sertleştirme turunda bir asimetriyi kapattı: korelasyonlu
istek zarfının **istek yarısı** (`RpcRequest`) `base.proto`'daydı ama
**yanıt yarısı** (`RpcResponse`) `game.proto`'daydı. Oysa zarfın iki
yarısının kuralı da core'un: id uzayı (0 reddi, PENDING duplike reddi,
cevaplanmış id'nin geri dönüşü), timeout süpürmesi ve "istek başına tam
bir cevap" değişmezi `gsb_core::rpc`'de yaşıyor. İkinci bir oyun crate'i
yanıt zarfını sıfırdan yazmak zorunda kalırdı ve core sözleşmenin
tamamını tek yerde ifade edemezdi. `RpcResponse` artık `base.proto`'da;
`game.proto` `import "base.proto"` ile `gsb.base.RpcResponse`'a referans
veriyor ve `Private.responses` (alan 3) değişmedi.

**Tel değişmedi.** Protobuf tel formatında tip adı yoktur: alan numaraları
(1..5) ve kodlanmış baytlar birebir aynı. Kilit:
`crates/gsb-demo/tests/wire_contract.rs` — beklenen baytlar taşımadan
ÖNCEKİ ağaçtan alındı ve taşımadan sonra aynen geçti (mutasyon kontrolü:
`bytes payload = 5` → `= 6` yapıldığında iki test de kırıldı).

Rust tarafı: oyun crate'inin (`gsb-demo`; kit bölmesinden beri
`gsb-kit` de) build script'i `prost_build`'e
`extern_path(".gsb.base", "::gsb_protocol::base")` verir — `gsb.base`
tipleri ikinci kez ÜRETİLMEZ, `gsb-protocol`'ün ürettikleri kullanılır
(iki kopya aynı baytı kodlar ama ayrı Rust tipi olurdu; kaldırılan
duplikasyon tam olarak bu). `base.proto`'nun dizini `gsb-protocol`'ün
`links = "gsb-base-proto"` anahtarı üzerinden `DEP_GSB_BASE_PROTO_DIR`
olarak dependent'a taşınır. `gsb_core::rpc::RpcReply` →
`gsb.base.RpcResponse` dönüşümü de core'da (`From` impl'i): oyun crate'i
alan eşlemesini ve proto3'ün dayattığı `u16 → u32` genişletmesini elle
yazmaz.

**Elenen alternatifler:**

1. **Bayt-birebir duplikasyon + uygunluk testi** (zarfı iki dosyada da
   tanımla, testle kilitle). Görev tanımının açıkça izin verdiği geri
   dönüş yoluydu; gerekmedi. Cross-crate proto import'u `extern_path` ile
   sorunsuz çalıştı, build sırası problemi yok (build script yalnız .proto
   DOSYASINI ister, derlenmiş crate'i değil). Duplikasyon her oyun
   crate'inde bir kopya daha demekti — kapatılan asimetrinin aynısı.
2. **Göreli include yolu** (`../gsb-protocol/proto`). Bu workspace'te
   çalışır, `gsb-protocol` registry'den geldiği anda kırılır — ki README'nin
   "yeni oyun = yeni bir oyun crate'i" senaryosu tam olarak odur. `links` +
   `DEP_*` cargo'nun bu iş için tanımlı mekanizması.
3. **`RpcResponse`'a kendi opcode'unu vermek** (core'un kendi frame'iyle
   cevaplaması). Zarfın yeri düzelirdi ama teslim modeli bozulurdu: cevap
   şu an bağlantı başına tek tick'lik `Private` frame'ine biniyor
   (sınırlı, atfedilmiş, ek kanalsız). Ayrı frame = tick başına ikinci
   frame + yeni opcode = tel değişikliği. Görev "sahiplik refactor'ü, tel
   refactor'ü değil" diyordu.
4. **`Private`'ı da base'e taşımak.** `Private` gerçekten oyun mesajı:
   `InputAck` ve `WorldSnapshot` oneof kolları oyun tipleridir. Sahiplik
   kuralı (yukarıda) onu `game.proto`'da tutar. *(gsb-kit Faz 2: zarf —
   `Private`, `InputAck`, `WorldSnapshot` — artık kit'in `kit.proto`'sunda
   (`gsb.kit`, kayıt ve hücre gövdeleri opak `bytes`); `game.proto`
   `InputAck`'i oradan alıyor, `WorldSnapshot` ile `Private`'ın tipli
   aynasını taşıyor. Baytlar aynı: KIT-ARCHITECTURE §5 ve §10 "Faz 2
   sonucu".)*

### 5.3 Emekli numaralar: `reserved` ve emekli opcode'lar

Kural: **kullanılıp kaldırılan hiçbir alan numarası sessizce geri
dönüştürülemez.** Bir alan silindiğinde aynı commit'te numarası VE adı
`reserved` edilir, gerekçe olarak silen commit sayılır. Spekülatif aralık
rezerve edilmez — gerekçesiz `reserved` numarayı boşa harcar.

Protokol sertleştirme turunda iki `.proto` dosyasının tüm geçmişi
tarandı (`git log -p -- crates/*/proto`):

- **`base.proto`: hiç alan kaldırılmamış.** Dosyaya dokunan her commit
  (88b0544, 3b9e5e3, 483e3a2, 5c9ac08, f13c6fb) yalnız EKLEMİŞ. Rezerve
  edilecek numara yok; dosyanın başına bu denetimin sonucu yazıldı ki
  bir sonraki bakıcı geçmişi yeniden çıkarmak zorunda kalmasın.
- **`game.proto`: tek gerçek kaldırma, `EntityState.version = 4`**
  (2ac28d2 — "Per record, the version field is dropped (21 → 16 bytes)";
  kaynağı olan `EntityVersion`/`bump()` ECS component'i aynı iş
  hattında silindi). Rolünü devralan `EntityRecord`'da `reserved 4;` +
  `reserved "version";`. Kapı derleme zamanında: alan 4 geri eklenirse
  protoc `Field "version" uses reserved number 4` ile build'i kırar.
  ROADMAP delta yayını için entity başına versiyonu geri getirmeyi
  düşünüyor — geldiğinde YENİ numara alır.

Aynı sınıf tehlike **opcode uzayında** da var ve orada `reserved`
anahtar kelimesi yok. 2ac28d2 üç opcode'u emekli etti ve birini (1003,
`ENTITY_STATE` → `WORLD_SNAPSHOT`) yeniden kullandı — tam olarak bu
kuralın engellemek istediği şey. Kalan ikisi artık
`gsb_demo::op::RETIRED` listesinde (1001 `ENTITY_SPAWNED`, 1002
`ENTITY_REMOVED`) ve `wire_contract.rs::retired_opcodes_stay_out_of_
the_message_table` bunların `MessageTable`'a kaydedilmesini kırıyor.
Yeni bir oyun crate'i kendi `RETIRED` listesini tutar.

### 5.4 `ErrorCode`: yorum tablosu değil, makine-kontrollü enum

ERROR kodları `base.proto`'da `message Error`'un üstünde bir **yorum
tablosu**ydı (1..12). Rust tarafı bu numaraları `gsb-core`'un bağlantı
aktöründe tam sayı literalleri olarak — üstelik iki ayrı `_ =>`
catch-all'ın arkasında — taşıyordu; loadgen ve örnek istemci ise kendi
`match e.code { 8 => …, 9 => … }` kopyalarını tutuyordu. Üç yerin elle
uyumlu kalması gerekiyordu.

Artık `enum ErrorCode` gerçek bir proto enum'ı: her istemci dili için
ÜRETİLİYOR, numaralandırma protoc tarafından kontrol ediliyor ve
`Error.code` alanının tipi `ErrorCode`. **Anlamların hiçbiri
değişmedi**; **tel de değişmedi** (proto3 enum'ı `uint32` gibi varint,
1..12'de bayt-birebir aynı — `error_code::the_code_field_encodes_exactly
_as_the_old_uint32_did` bunu 0..=12 için tek tek doğruluyor).

**Sıfır değer ve ileri uyumluluk** (proto3 enum'ı sıfır değer
zorunlu kılar; mevcut uzay 1'den başlıyordu):

- `ERROR_CODE_UNSPECIFIED = 0` **asla gönderilmez.** İki eşleme de
  (`ProtoError::wire_code`, `CoreError::wire_code`) onu üretemez ve iki
  test bunu kilitler. Sunucunun 0 ALMA durumu yok: ERROR gelen bir
  base-band opcode'u değil, bu yüzden sunucuya gönderilen bir `Error`
  çerçevesi `UNKNOWN_OPCODE` ile yanıtlanır ve ihlal bütçesine yazılır.
- **İstemci kuralı:** tanımadığı bir kod (gelecekteki bir numara) ya da 0
  → `ERROR_CODE_OTHER` gibi davran. `message` her zaman dolu, onu göster;
  döngüde tekrar deneme; bilinen bir kodun kararına EŞLEME. Bağlantının
  yaşayıp yaşamadığı **koddan çıkarılmaz** — onu taşıma söyler (yalnız
  `SERVER_CLOSED`, `ROOM_DESTROYED` ve `SERVER_STOPPING` kapanışla gelir,
  ve sokete güvenen istemci gelecekteki bir kapanış kodunu bilmeden de
  doğru işler — 14'ü tanımayan istemcinin 14'ü işleyişi tam olarak bu).
- proto3 enum'ları AÇIK: tanınmayan numara ham `i32` alanında korunur,
  düşmez. Bu yüzden istemci gerçek numarayı loglayıp sınıfı OTHER olarak
  işleyebilir (`an_unknown_code_survives_decoding`).

**Sızıntıya karşı kapı derleme zamanında:** iki eşleme de TÜKETİCİ
(exhaustive) `match`. Yeni bir `CoreError` ya da `ProtoError` varyantı
eklemek kodu **derlenmez hale getirir** — mutasyon kontrolüyle
doğrulandı (`non-exhaustive patterns: ... not covered`). Bir testten
güçlü: unutulan varyant CI'ya bile ulaşamaz. `base::Error::new(code,
message)` sunucu tarafında hata üretmenin tek yolu, yani hiçbir çağrı
yeri çıplak sayı yazamaz.

**Elenen alternatifler:**

1. **Alanı `uint32` bırakıp enum'u yalnızca belge/sabit olarak eklemek.**
   Numaralandırmayı yine de protoc kontrol ederdi ama üretilen istemci
   tipli bir alan almazdı ve `match e.code { 8 => … }` kopyaları
   kalırdı — yani turun kapatmak istediği elle-uyum sorunu sürerdi.
2. **Rust tarafında `#[repr(u32)]` kendi enum'umuz.** Tel sözleşmesini
   `.proto`'nun dışına taşırdı; Unity tarafı yine elle kopyalardı. Tek
   doğruluk kaynağı `.proto` olmalı.
3. **Kodları yeniden numaralandırmak / sınıfları birleştirmek** (ör. 3'ün
   iki auth hâlini ayırmak, `_ => 4` düşenlerine 6/7 vermek). Tel
   değişikliği olurdu; görev "her mevcut sayısal anlam aynı kalacak"
   diyor. `CoreError::wire_code` bu yüzden 4'e düşen on varyantı
   catch-all yerine TEK TEK sayar: davranış aynı, gözden kaçma imkânı
   yok.

### 5.5 Protokol sürümü: AUTH'a binen tek alan

**Karar: sürüm alanı EKLENDİ** — `Auth.protocol_version = 3`, sabit
`gsb_protocol::PROTOCOL_VERSION` (bugün `1`) ve yeni bir reddetme sınıfı
`ERROR_CODE_PROTOCOL_VERSION = 13`. Yeni round trip yok, yeni opcode yok,
yeni frame yok, yeni durum yok.

**Neden ertelenmedi.** Bu bir varsayım değil; bu depoda **iki kez oldu**:

1. `0888441` — `MoveTo` ve `EntityRecord` koordinatları `sfixed32` →
   `sint32`. Aynı alan numarası, FARKLI wire type (I32 → varint). Eski
   istemci sessizce yanlış ayrıştırır.
2. `2ac28d2` — opcode `1003` `ENTITY_STATE`'ten `WORLD_SNAPSHOT`'a
   geçirildi. Aynı numara, bambaşka mesaj.

İkisinde de sunucunun elinde karşıdakinin hangi wire'ı konuştuğunu
anlamanın hiçbir yolu yoktu.

**Ve asıl gerekçe: geciktikçe değersizleşiyor.** Alan bugün eklenirse
"göndermeyen" = tek bir sınıf, bugünden ÖNCEKİ istemciler (`0` =
sürümsüz/legacy). İki yıl sonra eklenirse iki yıllık istemci de `0`
gönderir ve sunucu "eski" ile "yeni ama sürümsüz"ü ayıramaz — tam olarak
alanın çözmesi istenen belirsizlik. Bir *base*'in çatallanacağı ilk gün
bu alanın en ucuz olduğu gündür.

**Sözleşme (tamamı):**

- `Auth.protocol_version` (alan 3, `uint32`). Yeni proto3 alanı =
  toplamalı: göndermeyen istemcinin ürettiği baytlar **birebir aynı**
  (varsayılan 0 kodlanmaz) — `wire_contract.rs` bunu kilitliyor.
- `0` = **sürümsüz (legacy)**. KABUL EDİLİR; sunucu `warn` seviyesinde
  bir kez loglar. Reddetmek mevcut her istemciyi (örnek istemci, loadgen,
  Unity tarafı) bir anda kırardı ve alanın amacı uyumluluk KURMAK.
- `protocol_version == PROTOCOL_VERSION` → kabul.
- Başka her şey → `ERROR_CODE_PROTOCOL_VERSION` (13). Mesaj iki sayıyı da
  taşır ("server speaks N, client presented M") ki istemci neye
  yükselteceğini bilsin.
- **Bağlantı YAŞAR** ve durum `WaitingAuth` kalır — bilet reddiyle (10)
  aynı aile: çerçeve iyi biçimli, bu bir protokol İHLALİ değil, o yüzden
  ihlal bütçesine **yazılmaz**. Uyumsuz istemci oturumu düzeltemez ama
  bunun için yeni bir temizleme mekanizmasına da gerek yok: pre-auth
  frame bütçesi (§3.3) ve idle timeout kimliği doğrulanmamış bağlantıyı
  zaten sınırlıyor. **Sıfır yeni koruma makinesi** — minimallik testi
  budur.
- Politika **tam eşitlik**, aralık değil. Tetikleyici: ikinci bir
  protokol sürümü gerçekten yayınlandığında min/max aralığı (ya da
  özellik pazarlığı) tartışılır. Önce veri. *(Kapandı — aşağıda "Base
  protokol evrim kuralı", kullanıcı kararı 2026-09-27.)*

**Base protokol evrim kuralı (kullanıcı kararı, 2026-09-27 — BACKLOG E5).**
İki protokol katmanı ayrıdır:

1. **Motorun base protokolü** — oyundan bağımsız, her istemcinin konuşmak
   zorunda olduğu çerçeveler: AUTH, JOIN, HEARTBEAT, ERROR kodları, RPC
   zarfı ve kit zarfı (`kit.proto`). `Auth.protocol_version` YALNIZ bu
   katmanın sürümüdür. **Geriye dönük uyumluluğu motorun
   sorumluluğudur**: motor base protokolünü kırarsa onu kullanan her
   oyunun eski istemcileri aynı anda kırılır — oyuncular istemcisini
   anında güncellemez, oyun geliştiricisinin motoru güncelleme sebebi
   çoğu zaman protokolle ilgisizdir.
2. **Oyunun protokolü** — oyunun opcode'ları, mesajları, kayıt biçimi ve
   onların sürümü: **oyunun işi** (BACKLOG E7). Motor bu katmanın
   sürümüne karışmaz; bir oyun kendi sürümünü kendi JOIN'inde ya da ilk
   mesajında taşır.

Motorun kuralları:

- **Varsayılan: toplamalı değişiklik.** Yeni proto3 alanı, eski istemcinin
  bilinmeyen sayıp atlayacağı yeni kod (ör. ERROR 14 → eski istemcide
  OTHER), oyunun opt-in açtığı yeni biçim (ör. `kit.proto` `records = 6`)
  serbesttir ve `PROTOCOL_VERSION`'ı ARTIRMAZ. Bugüne kadarki her değişiklik
  böyleydi; sürüm hâlâ `1`.
- **Uyumsuz değişiklik kaçınılmazsa sürüm artar** ve sunucu bir geçiş
  dönemi boyunca yeni sürümle birlikte en az bir öncekini de konuşur
  (N ve N−1).
- **Uyuşmazlık temiz tespit edilir**: ERROR 13 iki sayıyı da taşır
  (mevcut); istemci neye yükselteceğini bilir.
- **Kabul politikası dağıtımındır**: eski sürümün ne kadar süre kabul
  edileceğini operatör/oyun yayıncısı config'le verir
  (`min_protocol_version`); motor N−1'i konuşabilir, ama "bütün
  istemcilerim güncellendi, eskiyi reddet" kararını yalnız dağıtım
  verebilir.

Bugün kod değişmez: `min_protocol_version` ve N−1 desteği motorun İLK
uyumsuz base değişikliğiyle birlikte yapılır (BACKLOG E5 tetikleyicisi).
Bir PR base çerçevelerine dokunuyorsa önce "toplamalı mı?" sorusuna cevap
verir; cevap hayırsa bu kural uygulanır.

**Elenen alternatifler:**

1. **Ertelemek** (ROADMAP'e tetikleyiciyle yazmak). Reddedildi: yukarıdaki
   "geciktikçe değersizleşir" argümanı. Maliyet bir alan + bir enum
   değeri + üç test; ertelemenin maliyeti kalıcı bir belirsizlik sınıfı.
2. **Ayrı bir HELLO/VERSION opcode'u ve round trip'i.** Her bağlantıya
   bir RTT ve bir durum daha ekler; AUTH zaten ilk frame ve zaten
   reddedilebilir bir kapı. Sürüm bilgisinin oraya binmesi bedava.
3. **Sürümü frame başlığına koymak** (`[len][u16 op][u8 ver][payload]`).
   Her frame'de 1 bayt × her bağlantı × her tick — ve daha kötüsü, bu
   BİR WIRE DEĞİŞİKLİĞİ olurdu, yani tanıtmak istediği sorunun ta
   kendisini yaratırdı. Sürüm oturum başına bir kez söylenir.
4. **`AuthResult`'a da sunucu sürümünü eklemek.** Yalnız BAŞARI yolunda
   bilgi taşır — istemcinin zaten bildiği durum. Asıl gereken bilgi
   (sunucu hangi sürümü konuşuyor) reddetme mesajında ve orada.
5. **Uyumsuzlukta bağlantıyı KAPATMAK** (kod 9 ailesi). Sessiz/ani
   kapatma istemciyi reconnect-backoff döngüsüne sokar — kod 8'in
   gerekçesinde zaten belgelenmiş desen. Yaşayan bağlantı + pre-auth
   cap'ler daha ucuz ve daha az yeni kural.
6. **Sürümü config'ten okunur yapmak.** Tetikleyicisiz esneklik: bugün
   tek bir doğru değer var ve o `gsb-protocol`'ün sabiti.

### 5.6 Kapanış bildirimleri: sunucu durdurma (ERROR 14) ve reddedilen akış (ERROR 9)

**Sorun (BACKLOG B12, B13).** Sunucunun kendi başlattığı iki kapanış
istemciye SESSİZ görünüyordu: `ServerHandle::stop()` (bağlantı aktörünün
`ConnIn::Shutdown` kolu yalnızca döngüden çıkıyordu) ve taşımanın bayt
akışını reddetmesi (`stream_rejected`: `max_frame_bytes` üstü kare,
çözülemeyen kare gövdesi, WS protokol ihlali, bozuk TLS kaydı). İstemci
"sunucu kapanıyor" (sonra ya da başka yere bağlan) ile "ağ koptu" (hemen
tekrar dene) arasında seçim yapamıyordu.

**Karar.**

- **Durdurma → yeni kod `ERROR_CODE_SERVER_STOPPING = 14`.** Bağlantı
  aktörü `Shutdown`'da son çerçeve olarak ERROR 14'ü kuyruğa bırakıp çıkar.
  Kod, istemcinin KARARINI taşır: "bu oturum hakkında hüküm yok, sunucu
  gidiyor; bu adreste resume da yok (park defteri süreçle ölür) —
  geri çekil, sonra ya da başka sunucuya bağlan". Toplamalı değişiklik:
  bir enum değeri + üç sabitleme testi (`error_code.rs`: numara,
  bitişiklik, kodlama). **`PROTOCOL_VERSION` artmaz** — onun kuralı
  (bkz. sabitin belgesi) yeni `ErrorCode` değerini açıkça hariç tutar;
  14'ü tanımayan istemci onu `OTHER` sayar, kapanışı soketten öğrenir ve
  doğru davranır (`base.proto`'nun ileri-uyumluluk kuralı).
- **Reddedilen akış → mevcut `ERROR 9`**, mesaj `stream rejected: <sebep>`.
  Bu, oturum hakkında bir sunucu hükmü; kod 9'un sınıfı tam olarak bu
  (idle, cap, bütçeler, supersede ile aynı aile). Yeni kod gerekmez.
- **İkisi de en-iyi-çaba ve BEKLEMESİZ.** Bildirim, çıkış kuyruğuna
  senkron `try_send` ile girer (`conn/actor/close.rs::try_notice`); diğer
  kod-9 kapanışlarının beklemeli `send_frame`'i DEĞİL. Kuyruk doluysa
  (istemci okumayı bırakmış) bildirim düşer, istemci yalnız kapanışı alır
  ve aktör yine anında çıkar. Beklemeli gönderim, okumayan her istemci
  için bir aktörü write-stall penceresi dolana dek (pencere kapalıysa
  sonsuza dek) park ettirirdi — duran bir sunucuda. Reddedilen akışta da
  aynı gerekçe: reddedilen baytları gönderen eş, beklenecek eş değildir.
  Doğrulama sayacı değişmedi: `stream_rejected` hâlâ bir kez sayılır,
  shutdown hâlâ sayılmaz (SECURITY §3.6).

**Sonraki ek: odanın girdi-boşta kapanışı (BACKLOG E6).** Tavan
(`max_idle_input_secs`) opt-in `afk_action = disconnect` altında üyeyi
politikaya verdikten sonra oda registry'den bağlantının kapatılmasını
ister (`RegistryMsg::CloseConn`, RECONNECT §16.1); registry kararı
`ConnIn::ServerClosed { IdleInput }` olarak iletir. Bildirim **mevcut
`ERROR 9`**, mesaj `input idle: no game input for N s (…)` — oturum
hakkında bir sunucu hükmü, kod 9'un sınıfı; yeni kod gerekmez (istemcinin
kararı değişmez: kapanış, park varsa resume). Ve **en-iyi-çaba,
beklemesiz** (`try_notice`), stop ve reddedilen akış gibi: kapanan üye
okumayı da bırakmış olması en muhtemel üyedir. Sayılır:
`server_closes{reason="idle_input"}` (yeni etiket, sona eklendi).
Varsayılan `afk_action = leave_room`'da tel değişmez (soket açık,
bildirim yok); bağlantı yine de odadan çıkmış olur — registry satırını
bir ayrılma gibi yerleştirir ve bağlantıya `ConnIn::LeftRoom` iletir
(B40, RECONNECT §16/§16.2): oyun kareleri `ERROR 6` alır, `JOIN` doğrudan
geçer.

**Sonraki ek: oyunun atma fiili (BACKLOG E8).** Oyun mantığı bir üyeyi
tick bağlamından atar (`TickCtx::kick`; kit: `gsb_kit::game::kick`);
oda üyeliği aynı politika yoluyla bitirir ve aynı `CloseRequest`'i
`ServerClose::Kicked` ile ister (RECONNECT §16.3). Bildirim yine mevcut
`ERROR 9`, en-iyi-çaba ve beklemesiz; mesaj `kicked: <oyunun gerekçesi>`
(256 bayta kesilir). Sayılır: `server_closes{reason="kicked"}` (sona
eklendi). Yeni kod, yeni kare yok; hiç atmayan oyunda tel aynı. Hüküm
bağlantınındır (B43, RECONNECT §16.4): istek dolu registry kutusunun
arkasında beklerken bağlantı yeniden katılmış olsa da kapanış ona düşer;
`room`+`entity` koruması yalnız tablo yerleşimini korur.

**Kapanış yolları, kapı kapı (önce → sonra).** Değişmeyenler: `idle_timeout`,
`violation_budget`, `preauth_budget`, `conn_cap`/`unauth_cap`,
`superseded` → ERROR 9 (beklemeli gönderim, stall penceresiyle sınırlı);
`room_gone` → ERROR 5; `idle_input` (E6) ve `kicked` (E8) → ERROR 9
en-iyi-çaba, beklemesiz; `write_stall`, `rel_dead`, `outbound_dead` →
bildirim YOK (bildirimi taşıyacak yol ölü — sayacın var olma sebebi).

| Kapı | `stop()` önce | `stop()` sonra | `stream_rejected` önce | `stream_rejected` sonra |
|---|---|---|---|---|
| TCP | sessiz FIN | ERROR 14 → FIN | sessiz FIN | ERROR 9 → FIN |
| TLS | sessiz close_notify/FIN | ERROR 14 → close_notify/FIN | sessiz | büyük kare: ERROR 9 → son; **bozuk kayıt: bildirim YOK** (TLS oturumu ölü, rustls fatal alert'ini göndermiş, yazma başarısız) |
| WS | boş kapanış çerçevesi (istemcide 1005) | ERROR 14 (binary mesaj) → kapanış çerçevesi **1001 "Going Away"** (B24; önceden boş; hükümlerde B30'dan beri 1008/1013 — aşağıda "WS kapanış kodu (B24, B30)") | kapının kendi kapanış çerçevesi (1002/1003/1007/1009) | **aynı** — kapanış çerçevesi BU kapının bildirimi; RFC 6455 §5.5.1 kapanıştan sonra veri çerçevesini yasaklar, soket yazıcı görevi arkasına düşen ERROR 9'u (ve fan-out artığını) atar |
| QUIC | endpoint `close(0)`: tüm bağlantılar ANINDA kapanır, akıştaki veri terk edilir | ERROR 14 → akış FIN'i (ACK beklenir) | sessiz; üstelik okuyucu bırakınca yazıcının düşüşü son tutamaçtı → anında kapanış | ERROR 9 → akış FIN'i (ACK beklenir) |
| rUDP | sessiz (FIN yok; istemci kendi canlılık saatine kalır) | ERROR 14 REL bandında, TEK datagram (yazıcı, kanal kapanınca kuyruğu boşaltıp çıkar — yeniden gönderim fırsatı pratikte yok; kayıpta istemci eskisi gibi kendi saatine kalır); **FIN yok — bildirim tek kapanış sinyali** | yol yok (rUDP'de `StreamRejected` üretilmez; bozuk datagram demux'ta düşer) | — |

İki kapının kapanış yolu bildirim inebilsin diye düzeltildi:

- **QUIC dinleyicisinin `close`'u artık endpoint'i kapatmıyor**
  (`set_server_config(None)`: yeni bağlantı reddedilir, canlılar aktör
  kaskadına kalır — TCP dinleyicisinin kabul edilmiş soketlere
  dokunmaması gibi). `stop()` dinleyicileri registry'ye `Shutdown`
  yolladıktan hemen SONRA kapatıyor; `Endpoint::close` her bildirimin
  önüne geçip hepsini terk ediyordu.
- **QUIC gönderme yarısının kapanışı ACK'i bekliyor**
  (`gsb-net/src/quic/send.rs`): bağlantı son akış tutamacı düşünce ÖRTÜK
  ve ANINDA kapanır, uçuştaki veri terk edilir. Okuyucu çoktan çıkmışsa
  (reddedilen akış, ya da aktör bittikten sonra hâlâ gönderen istemci)
  yazıcının bildirimden hemen sonraki düşüşü son tutamaçtı. Şimdi
  `finish` + `stopped()` (eşin bütün baytları onaylaması, akışı durdurması
  ya da bağlantının ölmesi). Sınırlı: pump `close`'u write-stall
  penceresi altında koşar (beklerken bayt kıpırdamaz, pencere keser);
  pencere kapalıysa quinn'in idle timeout'u (30 sn) bağlantıyı bitirip
  beklemeyi hatayla çözer. Yan kazanç: QUIC'teki diğer kod-9 bildirimleri
  de aynı yarıştan kurtuldu.
- **WS soket yazıcı görevi, kapanış çerçevesinden sonra veri çerçevesi
  yazmaz** (`ws/writer.rs`). Kural tel sırasını gören TEK yerde: okuyucu
  hatalı kapanışı önce kuyruğa bırakır, aktörün ERROR 9'u ve teardown'a
  kadar gelen fan-out arkasına düşer. `WsWriter::start_send`'de
  `closing` bayrağına bakmak yarışlı olurdu (kontrol-sonra-gönder ile
  okuyucunun işaretle-sonra-kuyrukla'sı arasında pencere).

**Sınırlı kapanış argümanı (S'nin kuralları korunur).** `stop()`'un
await'leri değişmedi: `registry.send(Shutdown)` (posta kutusu kapasitesi)
ve metrik toplayıcı (broadcast `Closed`) — `stop()` her zaman tamamlanır
(§9.1). Registry'nin `on_shutdown`'ı değişmedi (spawn'lu `ConnIn::Shutdown`,
odalara `post_stop`). Aktörün `Shutdown` kolu tek bir SENKRON `try_send`
ekledi — await yok, istemciye bağlı hiçbir şey yok; aktör eskisi kadar
hızlı çıkar. Bildirimi sokete taşıyan writer pump ve QUIC ACK beklemesi
`stop()`'un zincirinde değil (hiç olmadılar) ve kendi pencereleriyle
sınırlı. QUIC dinleyici kapatması senkron. Kalan bedel, açıkça: ikili
(`main`) `stop()` döner dönmez çıkar ve runtime'ı düşürür; o ana kadar
sokete inmemiş bir bildirim kaybolur. Tek küçük çerçeve boş bir sokete
mikrosaniyeler içinde iner, ama garanti değildir — bildirim en-iyi-çaba
olarak belgelidir (elenen 6).

**Elenen alternatifler.**

1. *Durdurmayı ERROR 9 + mesaj metniyle bildirmek.* Kod 9 "bu oturum
   hakkında hüküm"; istemci kararı farklı (idle → hemen tekrar dene, cap
   → sonra). Ayırmak için istemci insan-okunur `message`'ı eşlemek
   zorunda kalırdı — enum'ın (§5.4) bitirdiği sürüklenmenin ta kendisi.
   14'ün maliyeti bir enum değeri; eski istemci etkilenmez.
2. *Yeni opcode / GOODBYE mesajı* (`retry_after`, yönlendirme adresi).
   Yeni kare tipi ve tablo girdisi; taşıyacağı veri (ne zaman, nereye)
   kontrol düzleminin/orkestrasyonun politikası, motorun değil.
   Tetikleyici: sunucu-yönlendirmeli taşıma isteyen bir platform.
3. *`PROTOCOL_VERSION` artırmak.* Eşitlik politikası mevcut her istemciyi
   kilitlerdi; toplamalı bir kod için gereksiz.
4. *Yalnız taşıma-yerli bildirim* (WS 1001 "Going Away", QUIC
   CONNECTION_CLOSE uygulama kodu). TCP/TLS/rUDP'de karşılığı yok —
   en çok ihtiyaç duyan rUDP'de hiç yok — ve istemci iki bildirim biçimi
   öğrenmek zorunda kalırdı. (WS'in durdurmadaki kapanış çerçevesi
   B24'te 1001'e çevrildi — bildirimin YERİNE değil, ARKASINDAN; aşağıda
   "WS kapanış kodu (B24, B30)".)
5. *Beklemeli gönderim* (diğer kod-9 kapanışlarındaki gibi). Okumayan
   her istemci için bir park etmiş aktör; bkz. yukarı.
6. *`stop()`'ta boşaltma süresi* (N ms bekle, ya da yazıcıları bekle).
   `stop()` ya istemcilere bağlanır ya da her durdurmaya sabit gecikme
   ekler — S'nin "stop() her zaman, istemciden bağımsız tamamlanır"
   kuralına aykırı.
7. *QUIC'te `endpoint.close(14, "server stopping")`.* Anında kapanış
   akıştaki ERROR çerçevesini yine terk eder; istemciye ikinci bir
   bildirim biçimi.
8. *WS'te bayrağı `WsWriter::start_send`'de denetlemek.* Yarışlı (yukarı).
9. *QUIC'te `Connection` tutamacını yazıcıda saklayıp `stopped()`'tan
   sonra açıkça kapatmak.* Aynı sonuç, daha çok tesisat;
   `SendStream` sarmalayıcısı QUIC kapısının içinde kalıyor.

**Testler.** `gsb-core/tests/close_notice.rs` (aktör, taşımasız: ERROR 14
ve ERROR 9 gider; dolu kuyrukta ikisi de PARK ETMEZ — beklemeli gönderim
mutasyonunda 5 sn zaman aşımıyla düşer); `gsb-server/tests/stop_notice.rs`
(her kapı: TCP/TLS/WS/QUIC/rUDP istemcisi ERROR 14'ü kapının sonundan
ÖNCE görür — bildirimsiz kodda beşi de, eski endpoint kapatmasında QUIC
düşer; okumayan 8 istemciyle `stop()` hızla tamamlanır);
`gsb-server/tests/stream_rejected.rs` (TCP/TLS/QUIC büyük kare → ERROR 9
sonra son — bildirimsiz kodda üçü, ACK beklemesiz QUIC düşer; WS metin
mesajı → 1003 kapanış çerçevesi ve ARKASINDA hiçbir şey);
`gsb-net` `ws::tests::after_close` (kapı seviyesinde aynı kural — yazıcı
kuralı olmadan düşer); `gsb-protocol` `error_code` (14 sabitlendi — 15
mutasyonunda üç test düşer). Resume semantiği değişmedi (`reconnect.rs`
yeşil): bildirim yalnız çıkış kuyruğuna bir kare ekler.

**WS kapanış kodu (B24, B30).** WS kapısının kendi kapanışı — bağlantı
aktörü oturumu bitirdiğinde writer pump'ın sink'i kapatması — boş bir
kapanış çerçevesiydi; istemci bunu 1005 ("durum yok") okur, hiçbir şey
söylemeyen bir eşten ayırt edemez. Artık **1001 "Going Away"**, yalnız
durum kodu (`[0x88, 0x02, 0x03, 0xE9]`; gerekçe metni yok — RFC 6455
§5.5: kontrol yükü ≤ 125 bayt, gerekçe ≤ 123 bayt UTF-8; okuyucunun
hata kapanışları da gerekçesiz). Okuyucunun hata kapanışları
(1002/1003/1007/1009) ve istemcinin başlattığı kapanışın yankısı
değişmedi.

- *Kapsam:* bu kapanış yalnız `stop()`'ta değil, aktörün bitirdiği her
  oturumda gider (idle, bütçe, cap, supersede — ERROR 9/5'ten sonra).
  B24'te kapı sebebi bilmiyordu (aktörden kapıya giden tek şey kare
  kanalıydı) ve hepsi 1001'di; B30'dan beri kod sebebe göre (aşağıda).
  HÜKÜM (neden, ne yapmalı) yine önündeki ERROR karesinde (tek bildirim
  biçimi, elenen 4); kod yalnız kaba sınıfı söyler.
- *Yan düzeltme — ikinci kapanış yok:* sunucu önce kapattığında,
  istemcinin cevap kapanışı okuyucuda yine yankılanıyordu (soket
  yazıcı görevi `closing` sonrası yalnız VERİ çerçevelerini atıyordu):
  istemci ikinci bir kapanış çerçevesi alıyordu. RFC 6455 §5.5.1 yankıyı
  yalnız önce kapanış göndermemiş uca ister; yazıcı artık ilk kapanıştan
  sonra her çerçeveyi atar, yalnız okuyucunun `Shutdown`'ını uygular.

*Elenenler.* (a) *1000 "Normal Closure":* "bağlantının amacı yerine
geldi" demek — `stop()` için yanlış, B24'ün adını koyduğu durum tam
1001. (b) *Sebebe göre kod* — B24'te ertelendi (aktörden kapıya sebep
yolu yoktu), B30'da yapıldı (aşağıda). (c) *Gerekçe metni* ("server
stopping"): stop'u diğer sonlardan ayıramayan kapı için yanıltıcı
olurdu; hata kapanışlarıyla tutarlı olarak yok.

**Sebebe göre kapanış kodu (B30) — sözleşme değişikliği.** Yalnız
kapanış koduna bakabilen bir istemci (tarayıcının `CloseEvent`'i, bir
vekil günlüğü) "sunucu gitti — yeniden bağlan"ı "yaptığın yüzünden
kapatıldın — körce yeniden bağlanma"dan ve "sunucu dolu — sonra dene"den
ayıramıyordu. Artık WS kapısının kendi kapanışının kodu oturumun nasıl
bittiğine göre (`gsb-net/src/ws/close_code.rs`; RFC 6455 §7.4.1, 1013
için IANA WebSocket kapanış kodu kaydı):

| Oturumun sonu | Kod | Neden bu kod |
|---|---|---|
| sunucunun duruşu (`stop()`) | 1001 Going Away | RFC'nin kendi örneği ("sunucu kapanıyor"); ERROR 14 de aynısını söyler |
| hüküm yok (istemci bitirdi) ya da kapıya söylenmedi | 1001 | değişmedi; kapatan istemci okumaz (kendi kapanışının yankısı kazanır) |
| `room_gone` | 1001 | oturumun yaşadığı uç gitti — istemci hemen başka yere katılabilir |
| `outbound_dead` | 1001 | oturuma hüküm değil, ölü çıkış yolu (kapanış nadiren iner) |
| `idle_timeout`, `write_stall`, `rel_dead` | 1008 Policy Violation | sunucunun canlılık politikası BU oturumu bitirdi |
| `violation_budget`, `preauth_budget`, `stream_rejected` | 1008 | eş protokol politikasını çiğnedi (reddedilen akışta okuyucunun kendi 1002/1003/1007/1009'u önce kuyruklanır, o kazanır) |
| `idle_input`, `kicked` | 1008 | odanın / oyunun politikası üyeliği ve oturumu bitirdi |
| `superseded` | 1008 | "son oturum kazanır" politikası — istemci körce yeniden bağlanmamalı (yenisini devralırdı) |
| `conn_cap`, `unauth_cap` | 1013 Try Again Later | sunucu kapasitede: oturum bir şey yapmadı, sonraki deneme tutabilir |

1011 (Internal Error) kullanılmıyor: "sunucu çöktü" diyen bir hüküm yok
(ölen oda `room_gone`'dur ve paniği yok etmeden ayırmaz). 1000 (Normal
Closure) da yok: buradaki her kapanış istemcinin istemediği bir sondur.

*Sayaçlar koda göre bölünmez (F66).* Bu kapanış dolu ya da kapalı
kuyrukta teslim edilemezse kodu ne olursa olsun aynı iki sayaca düşer:
`ws_teardown_closes_unsent_{closed,stalled}` (B80; §12). F66'ya kadar
adları `ws_going_away_unsent_*`'tı — B24'te kapanış hep 1001'di, B30'dan
beri ad yalnız bir kodu söylüyor, sayılan şey hepsiydi. Bölmek (koda göre
üç seri) elendi: kayıp kuyruğun hâli (kapalı / pencere doldu) ile ilgili,
kodla değil; hükmün sebebi zaten `server_closes{reason}`'da.

*Sebep yolu.* Bağlantı aktörü sebebi bilen tek yer; kapı yalnız çıkış
kanalının kapandığını görür. Kapı isterse uç noktaya (`Endpoint::
with_end_notice`) bir oneshot'un gönderen yarısını koyar; kabul döngüsü
onu `take_end_notice` ile alıp aktöre verir (`ConnectionActor::
with_end_notice`); aktör `run`'ın sonunda, çıkış göndericisini düşürmeden
ÖNCE `SessionEnd` (`Client` / `Stopped` / `Verdict(ServerClose)`) yollar
— kapı çıkış kanalının kapanışını gördüğünde sebep oradadır. WS kapısı
alıcıyı yazıcının `Teardown`'ında tutar ve kapanış slotu alındığında
`try_recv` ile okur; söylenmemişse 1001. Diğer kapılar uç noktaya bir şey
koymaz (TCP/TLS/QUIC/rUDP'nin kendi kapanış kodu yok). Kilit yok, yeni
await yok: tek bir oneshot gönderimi, tek bir `try_recv`.

*Uyumluluk.* İstemci teli yalnız WS kapanış çerçevesinin iki durum
baytında değişir (hükümle biten oturumda `03 E9` yerine `03 F0` ya da
`03 F5`); ERROR karesi (9/14), içeriği ve sırası aynı; duruşun ve
istemci sonunun 1001'i aynı. 1001 bekleyen bir istemci hükümle kapanan
oturumda 1008/1013 görür — kodu "sunucu gitti" diye okuyan istemci için
amaçlanan değişiklik budur. `gsb-client` kodu olduğu gibi gösterir
(`ws_close`), yorumlamaz.

*Elenenler.* (1) *Sebebi kare kanalında taşımak* (özel bir opcode'lu
son kare): her kapı onu ayıklamak zorunda kalırdı, unutulursa tele
çıkar; odalar aktörden sonra da kanala yazar, "son kare" yoktur.
(2) *Kapının ERROR karesini çözmesi* (son ERROR'un kodundan seçmek):
katman ihlali, opak eşlemede imkânsız, dolu kuyrukta düşen bildirim
(`close_notices_dropped`) kodu sessizce 1001'e çevirirdi. (3) *Paylaşılan
atomik* (aktör yazar, kapı okur): oneshot aynı işi kanal deyimiyle yapar.
(4) *Kod eşlemesini çekirdekte tutmak:* kod WS'e özgü, kapının kararı;
çekirdek yalnız sebebi söyler.

*Testler (B30).* `gsb-net` `ws::close_code::tests` (eşlemenin tamamı,
her kodun gönderilebilir oluşu); `ws::tests::verdict_close` (kapı:
aktörün son karesi bayt bayt aynı, sonra politika hükümlerinde
`88 02 03 F0`, tavanda `88 02 03 F5`, duruşta ve söylenmeyen sonda
`88 02 03 E9`); `gsb-core/tests/end_notice.rs` (aktör duruşu `Stopped`,
hükmü `Verdict`, istemci sonunu `Client` olarak söyler);
`gsb-server/tests/ws_close_code.rs` (uçtan uca: dört tanımsız opcode →
ihlal bütçesi → ERROR 9 → 1008; kabul döngüsünün tesisatı olmadan
düşer). Önce kırmızı: kodu sebepten bağımsız 1001 yapmak (= B30 öncesi)
kapı testlerini, tesisatı çıkarmak uçtan uca testi düşürür. Mutasyonlar:
tavanı 1008'e, duruşu 1008'e eşlemek, aktörün hükmü `Client` diye ya da
duruşu `Client` diye söylemesi, WS uç noktasının bildirim istememesi —
hepsi öldü. `ws_going_away.rs` ve `ws_client.rs`'in duruş testleri
(1001) değişmeden yeşil.

*Testler.* `gsb-net` `ws::tests::going_away` (kapı: aktörün son karesi,
sonra bayt bayt `88 02 03 E9`, istemci cevapladıktan sonra akış sonu —
ikinci kapanış yok; kapanış çerçevesi RFC sınırlarında);
`gsb-server/tests/ws_going_away.rs` (uçtan uca: `stop()` → ERROR 14 →
1001 → cevap → akış sonu; kendi ham WS istemcisiyle, çünkü sabitlenen
şey kapanış çerçevesinin baytı). Mutasyonlar: boş kapanış → iki test
de düşer; kapanıştan sonra kontrol çerçevesini atmamak → ikisi de
(ikinci kapanış) düşer. B80: `ws::tests::teardown` — dolu kuyrukta
kapanış slot bekler ve karelerin arkasından gider; sıkı soketle uçtan
uca, okumayan eş yeniden okuyunca bütün kareleri ve SONRA 1001'i alır
(önceden akış kapanışsız biterdi).

**Loadgen (B14) — yapılmadı, neden.** Kapanışları istemci tarafında
SEBEBE göre sınıflamak, kod-9 `message`'ını anahtar yapmayı gerektirir:
sözleşmesi insan-okunur (base.proto) ve üç crate'te altı ayrı yerde
yazılıyor — biri yeniden ifade edildiğinde sessizce kayar. Makine-okunur
yapmak ya her mevcut kod-9 mesajının baytlarını değiştirir ya da
`Error`'a alan ekler — ikisi de bu turun "yalnız sabitlenmiş bildirim
kareleri" kapısının dışında, ve gereksiz: sunucu sayacı
`server_closes{reason}` sebep başına kesin, loadgen onu istemci
sayılarının yanında basıyor (sıfır değilse WARNING). İstemci tarafı bir
sınıflama ancak onun gürültülü bir alt kümesi olabilir (stall'a düşen
istemci bildirimini hiç almaz — sayacın var olma sebebi). Loadgen KODA
göre sınıflıyor (8 → `join_rejected`, 9 → `cap_rejected`/`budget_rejected`,
14 → ileri-uyumluluk kolundan `errors`: koşan bir istemcinin altında
duran sunucu, başarısız bir koşudur; orkestratör sunucuyu istemcilerden
3 sn sonra durdurduğu için normal koşuda görünmez). Tetikleyici:
istemci-başına atıf isteyen bir ölçüm (hangi istemci döküldü) — o gün
`Error`'a toplamalı bir sebep alanı (`ServerClose` indeksi).

**Loadgen'in kendi `errors`'ı sebebe göre (B88).** Kapanışlar yine
sunucunun sayacında; ama istemcinin `errors` dediği şey artık tek sayı
değil: `RESULT` ve `CLIENT` satırlarında her sebep kesin adıyla, sıfırken
de (`errors=` toplamlarının hemen ardından, eklenen anahtarlar):
`errors_not_in_room` (kod 6 — oturum odada değilken okunan oyun/RPC
karesi), `errors_other_code` (sınıflanmayan her kod; ileri-uyumluluk
kolu, 14 dahil), `errors_bad_snapshot` (görünümün reddettiği snapshot),
`errors_bad_private` (reddedilen ya da RPC cevabı da taşımayan boş
private), `errors_connect_failed` ve `errors_empty_frame` (churn).
`errors` bunların toplamı (orkestratör `CLIENT` satırında eşitliği
doğrular). Loadgen'in metrik teli değişmedi (anahtarlar istemcinin).
B88'in kendisi: istemci girdiyi JOIN cevabından önce yollamaz (rUDP'de
kayıplı oyun bandı güvenilir JOIN'i geçiyor, `NotInRoom` → `errors`, 1000
istemcilik fırtınada 49–114); yalnız oturan istemci LEAVE yollar; JOIN'in
(son tarihten sonra) ve LEAVE'in cevabı `PROTOCOL_WAIT` = 5 sn beklenir —
rUDP'nin canlılık sınırı (`REL_NO_ACK_FATAL`, ≥ 4 × `MAX_RTO`, §6):
taşıma LEAVE'i o ana dek yeniden yolluyor; eski 500 ms bir `MAX_RTO`'nun
yarısıydı. Bekleme istemcinin KENDİ bekleme süresiyle ölçülür (100 ms'lik
dilimler, dilim en çok kendi uzunluğu kadar sayılır): aç kalan ya da
donan istemci cevabı soketinde dururken vazgeçmez (F35 aç bırakmasında
sunucu 12 join/12 leave sayarken istemci `joined=5 left=3`, bir koşuda
`joined=0` diyordu — F51). `client/wait.rs`, `client/errors.rs`,
`client/view/run/end.rs`.

### 5.7 İstemci yapı taşı: `gsb-client` (BACKLOG B19)

**Sorun.** Sunucunun istemci yarısı bir yapı taşı değildi, kopyaydı:
çerçeve okuyucu/yazıcı ve oturum adımları (AUTH → JOIN → HEARTBEAT →
LEAVE, ERROR işleme) `gsb-loadgen` (`loadgen/wire.rs` + `client.rs`), örnek
istemci ve on bir sunucu test dosyasında (e2e, multi_listener, tls_e2e,
economy_rooms, game_module, hosted, server_stop, server_closes,
stop_notice/stream_rejected, write_stall, udp_fragmentation) ayrı ayrı
yazılmıştı; TLS bağlayıcı üç, QUIC istemci yapılandırması üç kez.
`gsb-net`'te hazır tek istemci yarısı rUDP'ninkiydi (`UdpClient`).

**Kopyalar arasında bulunan ayrışmalar.**

- **İptal-güvensiz okuma (hata).** Her kopya `read_exact(uzunluk)` +
  `read_exact(gövde)` okuyup onu `tokio::time::timeout` ile sarıyordu
  (loadgen 250 ms, örnek 200 ms). Pencere önekten sonra, gövdeden önce
  dolarsa önek kaybolur, sonraki okuma payload baytlarını uzunluk sanar —
  akış kayar. Yeni okuyucu (`FramedRead` + `LengthDelimitedCodec`)
  yarım kareyi tamponda tutar; test: yarıda iptal edilen okuma hiçbir
  şey kaybetmez (eski okuyucuyla düşer).
- **Boyut koruması tutarsız:** 4 MiB (çoğu), yok (economy_rooms,
  game_module, server_stop, stop_notice — bozuk bir önek 4 GiB'lık
  ayırma ister), 64 KiB (write_stall).
- **EOF tutarsız:** kimi `None`, kimi panik; kare ortasında biten akış
  sınır EOF'undan ayrılmıyordu. Şimdi: sınırda `Ok(None)`, kare içinde
  `UnexpectedEof`, koruma/kısa gövde `InvalidData`.
- **AUTH_RESULT denetimi:** kimi `ok`'u doğruluyor, kimi (game_module,
  server_stop, udp_fragmentation) hiç bakmıyordu. `auth_and_join`
  reddi `AuthRefused` olarak döndürür.
- **Loadgen (davranış korunarak raporlandı, RESULT değişmesin diye
  düzeltilmedi):** `run_client`'ta TCP EOF'u sonrası döngü yorumun
  dediği gibi kırılmıyor, `continue` ediyor (bir sonraki hamle yazımı
  hata verene dek boş döner); churn istemcisi AUTH'u
  `wire_in_bytes(AUTH_REQ, 8)` ile sayıyor (sabit 8 bayt, TCP'de bile
  rUDP formülü), JOIN baytlarını hiç saymıyor, gelen baytı `2 + payload`
  sayıyor; yorumu "TCP'de birleşik yazım" diyor ama AUTH ve JOIN ayrı
  yazımlar; `tls_connector` CA PEM'ini her bağlantıda yeniden okuyor.
  **→ B26 ve B27'de düzeltildi** (aşağıda "Loadgen düzeltmeleri").
  (Birleşik yazım yorumu `run_client`'a aitti ve orada doğru: AUTH +
  JOIN `send_batch` ile tek yazım; churn istemcisi AUTH'u ve her JOIN
  denemesini ayrı yazar — yeniden deneme için.)

**Karar.** Yeni motor crate'i `gsb-client` (oyun bilmez, politika taşımaz):

- `frame`: `encode`/`encode_into`/`wire_len`; `FrameRx` (iptal-güvenli,
  varsayılan 4 MiB koruma, `with_max`), `FrameTx` (`send` = yaz + flush,
  `send_batch` = tek yazım, `feed`/`flush`, `get_mut` sözleşme dışı
  baytlar için).
- `Conn`: `Stream { rx, tx }` (TCP, TLS, QUIC bi-stream, WebSocket —
  aynı kareler) ya da `Udp(UdpClient)`; `send`, `send_batch`, sınırlı
  `recv(window)` → `Recv::{Frame, Closed, Quiet}`, `into_split` (okuyucu
  ve yazıcı iki görevde — çoğullama yok). Açıcılar: `connect::{tcp,
  tcp_stream, udp, ws, ws_stream}` (`ws_stream` B29'da), `ws::{handshake, handshake_with_max}`,
  `tls::{certs_from_pem, client_config, connector, connect}`,
  `quic::{client_config, connect}` (güven kökleri çağırandan; sistem
  deposu yok; ALPN `gsb-net/1`).
- `session`: kareler (`auth_req`, `join_req`, `leave_req`, `heartbeat`),
  adımlar (`hello` = AUTH+JOIN boru hattı, `auth_and_join`, `auth`,
  `join`, `heartbeat_round`, `leave`, genel `reply`). Yanıt beklenirken
  gelen diğer kareler sırayla çağıranın `other` havuzuna gider (hiçbiri
  atılmaz). `Credentials { name, ticket }` resume anahtarıdır: aynı
  kimlik bilgileriyle yeni bağlantıda JOIN park edilmiş varlığı geri
  getirir (protokolün kendi yolu; ayrı bir token yok).
- `ServerError { code: ErrorCode, raw, message }`: ERROR karesi tipli;
  bilinmeyen numara `Unspecified` okunur, `raw` saklanır (base.proto
  ileri-uyumluluk kuralı). `ClientError::{Io, Closed, TimedOut, Server,
  AuthRefused, Decode}`. Bağlantının yaşayıp yaşamadığı koddan
  çıkarılmaz — taşıma söyler.
- Tel baytları değişmedi: dört eski kodlayıcı yazımı donmuş kopyalar
  olarak `session/tests/pins.rs`'te `encode` ile bayt bayt karşılaştırılır,
  AUTH/JOIN/LEAVE/HEARTBEAT kareleri literal bayt olarak sabitlenir.

**Kalanlar ve nedenleri.** `gsb-net` ws süitinin sahte istemcisi
(`ws/tests/client.rs`): `gsb-net` `gsb-client`'e bağımlı olamaz (döngü:
`gsb-client` → `gsb-net`; dev-bağımlılık döngüsü `gsb-net`'in ikinci bir
kopyasını derler, tipler eşleşmez — alternatif 5'teki durum) ve o
istemci bilerek bozuk bayt yazar (maskesiz çerçeve, RSV, uzunluk
kodlamaları) — sunucu tarafı protokol testinin aracı, yapı taşı değil.
`udp_rel_liveness`
`UdpClient`'ı doğrudan sürer (ACK okumasını taşıma düzeyinde denetler).
e2e'nin koruma akışları adım adım döngülerini korur (her biri belirli bir
sunucu tepkisini doğrular), ama bağlantı + kare kurucular artık
`gsb-client`'in. Loadgen'in kendi alma döngüleri ve bayt muhasebesi
ölçüm politikasıdır; bağlantı, çerçeve, kareler ve ERROR sınıflaması
`gsb-client`'ten.

**WebSocket yarısı (BACKLOG B25).** B19 WS yarısını bilerek dışarıda
bırakmıştı; üç sunucu testi (multi_listener, stop_notice/stream_rejected)
el yazması RFC 6455 istemcileriyle kalmıştı — her biri kendi el
sıkışması, sabit maske anahtarı ve `timeout` altında `read_exact`'i
(B19'un iptal-güvensizlik hatası) ile.

- *Yeni `Conn` varyantı değil, aynı `Conn::Stream`.* Kapının sözleşmesi
  her ikili mesajda tam BİR akış-teli karesi `[u32 LE uzunluk][u16 LE
  op][payload]` — TCP kapısının baytları, mesaj başına bir kare. WS
  bağlantısı bu yüzden altında WS konuşan yarılarıyla bir `Stream`:
  `FrameRx` sunucu çerçevelerini ayrıştırır, `FrameTx` her kareyi tek
  maskeli FIN ikili mesaj olarak yazar. `send`/`recv`/oturum adımları/
  `into_split` TCP'deki gibi çalışır. `Conn` ya da `Recv`'e varyant
  eklemek her kapsamlı `match`'i kırardı (loadgen `Conn`'u ve `Recv`'i
  kapsamlı eşler); kapanış ayrıntısı bu yüzden bir sorgu:
  `Conn::ws_close() -> Option<&WsClose { code: Option<u16>, reason }>`
  (`Recv::Closed` birim varyant kalır — her kapının sonu tek bir olay).
- *El sıkışma:* 16 baytlık OS-rastgele `Sec-WebSocket-Key`, `GET` +
  `Upgrade`/`Connection`/`Sec-WebSocket-Version: 13`/`Host`; 101'in
  `Upgrade`, `Connection` ve `Sec-WebSocket-Accept`'i denetlenir;
  istenmemiş alt protokol/uzantı reddedilir (kapı ikisini de kullanmaz);
  101'in hemen arkasındaki baytlar ilk okumaya kalır. `connect::ws(addr)`
  düz `ws://` (yol `/`); `ws::handshake(io, host, path)` herhangi bir
  akış üstünde — `wss://` bir `tls` akışı üstünde `handshake`'tir, ama
  gsb kapısının TLS biçimi yok (`"ws"` dinleyicisi TLS dosyası kabul
  etmez), bu bileşim gsb'ye karşı sınanmadı.
- *Maskeleme:* her istemci çerçevesi, çerçeve başına taze OS-rastgele
  anahtarla (RFC 6455 §5.3).
- *Okuma:* maskeli sunucu çerçevesi, RSV, bilinmeyen opcode, metin
  mesajı, parçalı/125 bayttan uzun kontrol çerçevesi, açık parçalı
  mesajın içinde yeni veri mesajı, tam bir kare olmayan zarf →
  `InvalidData`. Parçalı mesajlar birleştirilir (araya kontrol
  çerçevesi girebilir). Koruma kareninkiyle aynı (`max_frame_bytes`,
  varsayılan 4 MiB) + 4 baytlık zarf; aşan mesaj (birleşmiş toplam
  dahil) BAŞLIKTAN reddedilir, yükü beklenmez.
- *Kontrol:* ping'e yükünü taşıyan pong. `Conn::recv` onu pencere
  içinde hemen yazar; `into_split` sonrası okuma yarısı sınırlı bir
  kuyruğa (8; dolu ise düşer — §5.5.3 yalnız sonuncuya yanıtı yeterli
  sayar) koyar, yazma yarısı bir sonraki karesinden önce gönderir.
  Kapanış çerçevesi: `Recv::Closed` + `ws_close()`; yankı yalnız kodla
  (boş kapanışa boş), istemcinin başlattığı kapanışın yankısına yankı
  yok; kapanıştan sonra veri gönderimi `BrokenPipe`. Sonraki okuma
  sunucunun TCP sonunu bekler (`Closed`; beklerken `Quiet`) ve
  kapanıştan sonra gelen her bayt `InvalidData` — "kapanış çerçevesinden
  sonra hiçbir şey" iddiası (stream_rejected) göçte zayıflamadı.
  Kapanışsız TCP sonu da `Closed`, `ws_close() == None`.
- *İptal güvenliği iki yönde:* okuma durumu tamamen yapıda (tampon, açık
  mesaj, kapanış), tek `await` tamponlu okuma; yazma kodlanmış çerçeveyi
  ilk `await`'ten ÖNCE bütünüyle `pending`'e koyar, kısmi yazımlarla
  boşaltır — iptal edilen gönderim yarım çerçeve bırakmaz, kalanı
  sonraki yazım önce gönderir.
- *Bağımlılık:* yeni crate yok. `sha1` (kapının kendi el sıkışması),
  `getrandom` (rUDP çerez anahtarının OS okuyucusu) çalışma alanında;
  `base64` 0.22 `Cargo.lock`'ta zaten vardı (`pem` üzerinden) — çalışma
  alanı bağımlılığı yapıldı, kilitte yalnız `gsb-client`'in bağımlılık
  listesi büyüdü. Kapının el yazması base64'ü `pub(super)`; `gsb-net`'e
  dokunmamak için yeniden kullanılmadı.
- *Göç:* multi_listener (`FakeWsClient` ve `ws_auth_and_join` gitti; WS
  kapısı da `session::auth_and_join`'den geçer), stop_notice/client.rs
  (beş kapı da `Conn`; `End::WsClose` yükü `ws_close()`'tan kurulur,
  bir kez raporlanır — sonraki son TCP sonudur), stream_rejected (metin
  çerçevesi `FrameTx::ws_frame` ile). Hiçbir iddia değişmedi; test
  sayıları aynı (11, 6, 4).
- *B24 ile ilişki:* stop kapanışı bugün boş (`code: None`); paralel tur
  1001 yapıyor. `ws_client`'in stop testi ikisini de kabul eder
  (`None | Some(1001)`) — B24 birleşince `Some(1001)`'e sıkılaşır.

**Elenen alternatifler.**

1. *`gsb-net`'e istemci modülü.* `gsb-net` sunucunun taşıma katmanı;
   oturum adımları (kimlik bilgisi, AUTH/JOIN sırası, ERROR tiplemesi)
   protokol düzeyi istemci bilgisidir, taşıma değil. Ayrı crate
   `gsb-net`'in istemci yarısını (`UdpClient`) kullanır. Bedeli açıkça:
   `UdpClient` `gsb-net`'te yaşadığı için `gsb-client` `gsb-net`
   üzerinden `gsb-core`'u da çeker. Tetikleyici: çekirdeksiz bir
   istemci derlemesi (ör. wasm/mobil) — o gün `UdpClient` kendi
   crate'ine taşınır.
2. *`FrameIo` trait'i (WS test istemcisi de dahil her taşıma adımları
   kullanabilsin).* Tek tüketici test düzeneği; enum tek yer, trait bir
   soyutlama katmanı. Tetikleyici: ikinci gerçek (test dışı) taşıma.
3. *Oturum adımlarını bir istemci aktörü/durum makinesi olarak
   (yeniden bağlanma, backoff içeren).* Politika — oyunun/çağıranın.
4. *Görünüm eşleştirme yardımcısı (`gsb_kit::client` ile).* Tek
   tüketici (hosted `View`); kit'in istemcisi zaten tek satırla
   uygulanıyor. Tetikleyici: ikinci bir çağıran.
5. *Gerçek-sunucu testlerini `gsb-client`'e koymak.* `gsb-server`
   `gsb-client`'e bağlı; tersine dev-bağımlılık döngüsü crate'in ikinci
   bir kopyasını derler (`gsb-kit/src/manifest.rs`'in kaçındığı durum).
   Gerçek sunucu turu `gsb-server/tests/client_session.rs`'te.

**Testler.** `gsb-client` birim (17): çerçeve gidiş-dönüş, koruma
(sınırda kabul, üstünde önekten ret, varsayılan 4 MiB), kısa gövde reddi,
kare içi EOF = `UnexpectedEof`, yarıda iptal edilen okuma; senaryolu eşe
karşı adımlar (diğer kareler sırayla `other`'a, ERROR 9/14 tipli,
bilinmeyen kod 99 → `Unspecified` + ham 99, `AuthRefused`, EOF →
`Closed`, sessizlik → `TimedOut`); bayt sabitlemeleri.
`gsb-server/tests/client_session.rs` (7): TCP/TLS/rUDP/QUIC üzerinde
tüm adımlar; kapasite reddi ERROR 9 tipli; `stop()` ERROR 14 tipli (TCP
ve rUDP); aynı kimlik bilgileri aynı varlığı geri getirir.
B25: `gsb-client` WS birim (23, senaryolu sunucuya karşı): RFC accept
vektörü; istek başlıkları + 101 arkasındaki bayt; bağlantı başına taze
anahtar; yedi kötü yanıt + yanıtsız kapanış; her kare tek maskeli ikili
mesaj (kare başına taze anahtar); 7/16/64-bit uzunluklar; ping araya
girmiş parçalı mesaj + pong; `recv` penceresinde pong; bölünmüş
yarılarda pong sonraki kareden önce; `ws_frame`; kapanış kodu/nedeni +
tek yankı + kapanış sonrası gönderim reddi + TCP sonu; kapanıştan sonra
bayt reddi; boş kapanış; kapanışsız son; istemci kapanışının yankısı
yankılanmaz; bozuk kapanış; koruma (sınırda kabul, başlıktan ret,
birleşmiş toplam, varsayılan 4 MiB); on iki protokol ihlali (kalıcı
hata); mesaj içinde EOF; yarıda iptal edilen okuma ve gönderim.
`gsb-server/tests/ws_client.rs` (4, gerçek kapı): oturum adımları;
kapının 1003/1002/1007/1009 kodları; istemci kapanışının 1000 yankısı;
stop → ERROR 14 + kapanış çerçevesi. Mutasyonlar (maskesiz, anahtar
uygulanmaz, sabit anahtar, yalnız son parça, pong yok, `recv` pong
yazmaz, kapanış kaydedilmez, yankı yok, koruma yok, pencere altında
`read_exact`, iptalde `pending` düşer, kapanış sonrası bayt kabul) her
biri en az bir testi kırar.

**Loadgen düzeltmeleri (B26, B27 — 2026-09-26).** B19'un bilerek
dokunmadığı üç ölçüm kusuru; ikisi RESULT değerlerini BİLEREK oynatır.

- *Churn bayt muhasebesi (B26).* Churn istemcisi artık `run_client`'ın
  kuralıyla sayar: yazılan her kare (AUTH, her JOIN denemesi — yeniden
  denemeler dahil —, girdiler) ve okunan her kare (JOIN evresinin
  cevapları — sonuç, ERROR, araya giren kareler — dahil) bu teldeki
  gerçek boyutuyla, `frame_bytes`: akışta (TCP/TLS) uzunluk önekli kare
  `4 + 2 + yük`, rUDP'de datagram (REL denetim bandı `1 + 4 + 2 + yük`,
  RAW oyun bandı `1 + 2 + yük`). TLS kayıt ek yükü ve TCP/IP
  başlıkları iki istemcide de sayılmaz (ölçüm "istemcinin yazdığı/
  okuduğu kare baytı"dır, `server_*_bps` sunucu tarafının kendi
  sayacı). Önce: AUTH sabit `wire_in_bytes(AUTH_REQ, 8)` = 15 bayt (TCP'de
  bile rUDP formülü, gerçek yük uzunluğu yerine 8); girdiler de rUDP
  formülüyle (TCP'de kare başına 3 bayt eksik); JOIN hiç; gelen kare
  `2 + yük` (TCP'de 4, rUDP'de 1 ya da 5 bayt eksik); JOIN evresinde
  okunan kareler yalnız yük, sonuç ve ERROR hiç.
- *`run_client`'ın EOF döngüsü (B26).* Kare dışındaki her alım
  `continue` ediyordu: TCP EOF'undan (ya da okuyucunun reddettiği
  kareden) sonra her alım anında dönüyor, döngü bir sonraki girdi yazımı
  hata verene dek (bot bir şey göndermiyorsa son tarihe dek) boş
  dönüyor ve ölü sokete yazılan hamleleri `moves`/`bytes_out`'a
  sayıyordu. Artık akışın ölümü oturumu bitirir (`recv_wire` → `Dead`);
  sessiz pencere ve rUDP soket hatası (rUDP'nin bildirebildiği tek şey)
  yine döner.
- *TLS bağlayıcı (B27).* `--tls-ca` PEM'i koşu başında BİR kez okunup
  ayrıştırılır; `TlsOpts` bağlayıcıyı taşır (içi `Arc`), her istemci
  onun klonunu alır — her bağlantı (churn'ün her yeniden bağlanması
  dahil) aynı rustls istemci yapılandırmasını paylaşır. Sunucu adı
  dönüşümü bağlantı başına kaldı (ucuz; geçersiz ad bağlantı hatası
  olarak raporlanmaya devam eder).

**RESULT'ta oynayan alanlar.** Churn modunda `client_out_bps` ve
`client_in_bps` (iki taşımada). Düz modda (`run_client`): yalnız
sunucunun koşu ORTASINDA kapattığı oturumlarda (write-stall, cap,
bütçe, `--idle-timeout-secs` …) `moves` ve `client_out_bps` — ölü
sokete yazılan hamleler artık sayılmıyor; normal bir koşuda sunucu
istemcilerden sonra durduğu için hiçbir alan oynamaz. Ölçüm (release,
aynı makine, ÖNCE iki koşu / SONRA iki koşu; 32 çekirdek, yük ~8):

| Senaryo | Alan | önce (1 / 2) | sonra (1 / 2) | |
|---|---|---|---|---|
| `200 --duration 10 --write-stall-secs 0` | — | | | yalnız koşudan koşuya gürültü (`snap_total`, `client_in_bps` ±%1, adım süreleri …); hiçbir alan önce/sonra ayrışmıyor |
| `100 --duration 10 --write-stall-secs 0 --churn-secs 2 --disconnect-grace-secs 5` (TCP) | `client_out_bps` | 6 548 / 6 548 | 8 893 / 8 893 | **oynadı** (+%36: JOIN'ler, AUTH'un gerçek boyu, girdi başına +3 bayt) |
| aynı | `client_in_bps` | 1 931 736 / 2 099 472 | 1 851 490 / 1 970 829 | beklenen kayma kare başına +4 bayt (~+%0,6) — koşudan koşuya gürültünün (±%8) altında |
| aynı, `--transport udp` | `client_out_bps` | 6 502 / 6 416 | 7 014 / 6 861 | **oynadı** (+%7–8: JOIN'ler ve yeniden denemeleri, AUTH'un gerçek boyu); JOIN yeniden deneme sayısı koşuya bağlı |
| aynı | `client_in_bps` | 2 484 321 / 2 439 523 | 2 497 147 / 2 437 416 | kare başına +1 bayt (RAW) — gürültü içinde |

Diğer bütün alanlar önce/sonra aynı ya da yalnız iki "önce" koşusu
arasında da oynayan gürültü (churn'ün `resumed`/`fresh_joins`/
`resume_rejected_stale`'i, adım süreleri, `snap_total`).

*Testler.* `churn::tests::churn_bytes_are_the_frames_on_the_wire`
(betikli TCP eşi: reddedilen ilk JOIN, araya giren kare, oyun kareleri;
istemcinin sayımı eşin okuyup yazdığı baytla birebir — eski kodda
141 ≠ 198 ile düşer; beş kuralın her birinin tek tek mutasyonu da
düşürür); `client::view::run::tests::a_stream_eof_ends_the_client`
(JOIN sonrası kapanan eş: istemci EOF'ta biter — eski kod 6 sn sonra,
iki hamle yazıp); `client::tests::the_ca_is_read_once_per_run` (PEM
yüklendikten sonra silinir, iki el sıkışma yine tamamlanır — eski kod
"cannot read --tls-ca" ile panikliyor).

**Loadgen WS modu (B29 — 2026-09-26).** WS kapısı yük altında hiç
ölçülmemişti: loadgen'in `--transport`'u `tcp|udp` alıyordu.

- *Bayrak, her modda.* `--transport tcp|udp|ws`. Süreç içi ve `--serve`
  sunucusu `ws`'de bind adresinde TEK bir `"ws"` `[[listeners]]` girdisi
  açar (`loadgen/transport.rs`, `Transport::open_door`); `ws` eski skaler
  `transport` anahtarının değeri değil (sunucu onu yalnız dizi yazımı
  tutar), skaler `bind` varsayılanına döner — yoksa sunucu "skaler
  anahtarlar yok sayılıyor" uyarısı basardı. TCP ve rUDP skaler anahtarla,
  eskisi gibi. `--addr` istemcileri `gsb_client::connect::ws_stream` ile
  bağlanır: yeni yapı taşı, `tcp_stream`'in WS ikizi — çağıranın açtığı
  TCP soketi (yavaş okuyucunun 16 KiB alım tamponu ve MSS'i dahil),
  `TCP_NODELAY`, yükseltme, sahipli yarılar (genel `ws::handshake`
  `tokio::io::split` kullanır; ölçülen istemci TCP'ninkiyle aynı yarı
  biçimini taşısın diye). Orkestratör `--transport`'u iki çocuğa da
  iletir (hazırlık yoklaması TCP bağlanıp kapatma: WS kapısında bir
  "handshake failed" uyarısı, oturum ve sunucu kapanışı yok). Churn,
  `--stall-ms`, `--capture`, `--flood-id` WS'de çalışır; flood WS'de
  her kareyi ayrı maskeli mesaj olarak besler (akış karesini sokete ham
  yazmak WS sözleşmesini bozardı). **Ret:** `--tls-ca` ile `ws` her
  modda kullanım hatası (çıkış 2, tek satır) — istemci yarısı `wss://`
  konuşabilir ama gsb kapısının TLS biçimi yok; bu ret `--tls-ca
  requires --addr`'dan önce denetlenir (kesin sebep o).
- *Bayt muhasebesi (karar).* WS'de `client_in_bps`/`client_out_bps`
  (`CLIENT` satırının `bytes_in`/`bytes_out`'u) her VERİ mesajını sokette
  durduğu boyla sayar: RFC 6455 başlığı (gövde ≤125 B ise 2, ≤65535 ise
  4, üstü 10 bayt) + istemcinin gönderdiğinde 4 baytlık maske anahtarı +
  içindeki uzunluk önekli kare (`4 + 2 + yük`). Gövde sınırı zarfın
  boyudur (kare `wire_len`'i), yükün değil. Sayılmayan: HTTP yükseltmesi,
  ping/pong/kapanış çerçeveleri, TCP/IP başlıkları — rUDP'nin çerez el
  sıkışmasını ve ACK datagram'larını, TCP'nin TCP/IP başlıklarını
  saymamasıyla aynı çizgi ("veri biriminin teldeki boyu"). Tek mesaj =
  tek kare varsayımı sözleşmedir: kapı ve `gsb-client` her kareyi tek
  FIN mesaj olarak yollar. `frame_bytes` artık yönü alır
  (`Dir::{In, Out}`; maske yalnız istemcinin yazdığında); TCP/rUDP
  sayımları değişmedi. Sunucu tarafı sayaçlar (`server_*_bps`,
  `out_bps_per_conn`) bağlantı aktöründe, kapıya verilen kare baytıyla
  sayılır — WS çerçevelemesi (TLS kaydı, rUDP başlığı gibi) onlarda hiç
  görünmez; ölçümde iki taşımada aynı çıktılar.
- *Modül bölünmesi:* `client.rs` 242 → 152 satır; bağlantının doğumu
  `client/connect.rs`, bayt muhasebesi `client/accounting.rs`.

*Testler (önce kırmızı, mutasyonlu).*
`client::accounting::tests::a_ws_message_is_its_header_the_mask_and_the_frame`
(iki yönde 125/126/65535/65536 baytlık gövde sınırları);
`...::ws_bytes_are_the_messages_on_the_wire` (betikli WS eşine karşı tam
bir `run_client` oturumu — yükseltme, ping, JOIN sonucu, iki uzunluk
sınırının iki yanında üç kare, LEAVE sonucu — düz ve flood istemci:
istemcinin `bytes_out`'u eşin okuduğu, `bytes_in`'i eşin yazdığı veri
mesajı baytına birebir eşit, pong sayılmaz; eski muhasebe 62 ≠ 98 ile,
eski flood kolu maskesiz çerçeveyle düşer);
`transport::tests::{each_transport_opens_its_door, the_spelling_round_trips}`;
`args::tests` (`ws` dört modda ayrışır, `--tls-ca` + `ws` reddi);
`child_args::tests::both_children_get_the_transport`;
`gsb-client` `connect::tests::ws_stream_upgrades_the_callers_stream`;
`gsb-server/tests/loadgen_ws.rs` (3, gerçek ikili): dört oyun süreç içi
WS kapısı üzerinden (`transport=ws`, connected = joined = left = N,
errors/server_closes/dropped/cap_rejected 0, akış, onaylanan numaralı
girdiler), orkestre demo WS koşusu (`mode=sep`, iki çocuk), `--tls-ca`
reddi iki modda. Ayrıştırıcıdan `ws` çıkarılınca üçü de düşer. Öldürülen
mutasyonlar: maske yok, üç uzunluk sınırı, taban başlık yok, WS'nin akış
gibi sayılması, flood baytı sayılmıyor, ret yok, kapı TCP açıyor, `bind`
sıfırlanmıyor, yükseltmesiz bağlantı, ayrıştırıcıda `ws` yok; `ws_stream`
için Host yok sayılıyor ve 101 arkasındaki bayt düşüyor.

*Ölçüm (WS ↔ TCP taban çizgisi).* Release, 32 çekirdek, süreç içi, 20 sn,
`--write-stall-secs 0`, bağlantı yığını birden (stagger 0); her senaryoda
TCP, WS, TCP, WS sırasıyla (iki çift; hücreler "1. / 2. koşu"). Komut:
`gsb-loadgen N [--game arena|mmo] --transport tcp|ws --duration 20
--write-stall-secs 0`. Yük: her koşudan önceki 1 dk ortalaması.

| Senaryo | Taşıma | Yük | `server_hz` | step p50/p90 fine (µs) | `out_bps_per_conn` | `client_in_bps` | `client_out_bps` | connect p50/p99 (ms) |
|---|---|---|---|---|---|---|---|---|
| demo 200 | TCP | 8,1 / 6,3 | 30,00 | 200/320 · 200/304 | 46 690 / 46 408 | 9 379 162 / 9 320 358 | 14 318 / 14 274 | 0/1019 · 0/1017 |
| demo 200 | WS | 7,3 / 6,0 | 30,00 | 216/328 · 224/336 | 46 428 / 46 452 | 9 353 883 / 9 358 702 | 21 451 / 21 459 | 14/1021 · 17/1020 |
| demo 500 | TCP | 5,3 / 4,4 | 30,00 | 800/1536 · 696/1312 | 119 393 / 119 405 | 59 800 932 / 59 799 537 | 35 141 / 35 123 | 1004/2039 · 1003/1066 |
| demo 500 | WS | 5,1 / 3,7 | 30,00 | 776/1400 · 672/1160 | 118 560 / 118 898 | 59 446 683 / 59 612 438 | 52 357 / 52 467 | 1011/1449 · 1061/2085 |
| arena 200 | TCP | 3,2 / 2,1 | 30,00 | 384/488 · 400/520 | 23 588 / 23 445 | 4 743 364 / 4 727 027 | 18 648 / 18 577 | 0/1006 · 0/1007 |
| arena 200 | WS | 2,6 / 2,1 | 30,00 | 400/520 · 376/488 | 23 426 / 23 372 | 4 739 061 / 4 726 058 | 25 777 / 25 785 | 16/1009 · 21/1010 |
| MMO 200 | TCP | 1,8 / 2,1 | 30,00 | 112/160 · 104/176 | 20 254 / 20 264 | 4 090 867 / 4 092 497 | 16 655 / 16 703 | 0/1059 · 0/1030 |
| MMO 200 | WS | 1,9 / 1,9 | 30,00 | 128/208 · 128/208 | 20 223 / 20 202 | 4 112 904 / 4 106 722 | 23 779 / 23 765 | 17/1046 · 20/1019 |

Her koşuda `joined = left = N`, `errors = server_closes = dropped = 0`.
Orkestre demo 500 (`--orchestrate 500 --procs 2`, aynı bayraklar; sıra
TCP, WS, TCP, WS; yük 17,7 / 13,1 / 9,9 / 7,1 — kardeş çalışma ağacının
derlemesi): step p50/p90 TCP 248/432 · 240/448, WS 256/416 · 272/416;
`server_cpu_s` TCP 1,7 / 1,7, WS 1,9 / 1,8; `dropped` TCP 116 / 116, WS
14 / 17 (katılma fırtınasında; WS kapısının bağlantı başına 64'lük kendi
yazıcı kuyruğu fan-out'un önüne tampon ekliyor); connect p50/p99 TCP
17/1024 · 20/1009, **WS 1065/1466 · 1062/1469**. *Bu orkestre sayıları
tek worker'lı çocuklar (B37 öncesi) koşulundadır.* **Varsayılan
worker'larla (B50, 2026-09-28, `73da266`, üç koşu, yük 3,5–5,0; medyan,
parantezde aralık):** step p50/p90 TCP 808/1216 (712–832 / 1200–1224),
WS 816/1272 (768–896 / 1208–1336); `server_cpu_s` TCP 4,6 (4,4–4,7), WS
4,9 (4,8–5,2); `dropped` 0 ve `sends_closed` 0 her koşuda; connect
p50/p99 TCP 31/1066, WS 32/1064 (WS'nin ~1 sn'lik p50'si B31'de gitti).
Aynı ağaç `--workers 1` ile TCP 352/632 · 296/528, `server_cpu_s` 2,3 ·
1,9, `sends_closed` 116 · 116 — adım ve CPU farkı worker sayısının,
116 tek worker zamanlamasının (RPC-CONTROL-PLANE §8.2 "B50"). Süreç
içi satırlar (tablo) sayım turlarından sonra yeniden koşuldu, üçer koşu:
demo 200 184/272 · `out_bps_per_conn` 46 556, demo 500 712/1256 ·
119 053, arena 200 408/560 · 23 491, MMO 200 128/176 · 20 303 —
tablodakilerle aynı bantta.

*Okuma.* (1) Tick yolu: `server_hz` hep 30; adım süreleri gürültü
içinde, yalnız MMO 200'de WS p90 iki çiftte de +32…48 µs (süreç içinde
kapının bağlantı başına ek yazıcı görevi aynı çekirdekleri paylaşıyor;
orkestre koşuda sunucu CPU'su +%6-12). (2) Bant: sunucu tarafı aynı
(`out_bps_per_conn` farkı ≤%1; orkestre ve demo 500'de WS −%1-3 —
geç bağlanan istemcilerin kısalan penceresi, aşağıda); `client_in_bps`'te
WS başlığı kare başına 2-4 bayt, ~1,5 KB'lık snapshot'larda %0,3 —
koşudan koşuya gürültünün altında (`client_in_bps`/`server_out_bps` oranı
demo 200 TCP 1,0044 ↔ WS 1,0073, MMO 1,0099 ↔ 1,0169). `client_out_bps`
WS'de +%38-50: küçük girdi karelerine (onlarca bayt) 2 bayt başlık + 4
bayt maske. (3) **Bulgu — bağlanma:** WS kapısı yükseltmeyi `accept()`'in
İÇİNDE yapıyor (`gsb_net::ws::transport`, `WsListenerHandle::accept`)
ve sunucunun accept döngüsü onu sırayla bekliyor: el sıkışmalar seri.
200 istemcide WS connect p50 14-21 ms (TCP 0); orkestre 500'de backlog
taşıyor, istemcilerin çoğu 1 sn'lik SYN yeniden gönderimini yiyor (p50
~1065 ms ↔ TCP 17-20 ms). Daha ağır hâli: yükseltme isteği göndermeyen
TEK bir TCP bağlantısı kapıyı `WS_HANDSHAKE_TIMEOUT` (10 sn) boyunca
kilitliyor — `--serve --transport ws` + boşta tutulan bir soket, sonra
20 `--addr` istemcisi: connect p50 = p99 = 9610 ms (aynı deney TCP
kapısında 0 ms). Başarısız bir el sıkışma ayrıca accept döngüsünü 100 ms
geri çekiyor ("accept error; backing off"). TLS kapısı aynı yapıda
(`tls.rs` `accept` içinde rustls el sıkışması) — kod okumasıyla; loadgen
süreç içi TLS sunucusu kuramadığı için ölçülmedi. Bu turda düzeltilmedi
(kapsam: ölçüm); BACKLOG'a yeni madde. **→ Düzeltildi (B31):** el
sıkışma artık bağlantı başına görevde, kapı başına sınırlı — §6 "El
sıkışan kapılar"; aynı deney 9612 → 0 ms, orkestre 500 WS connect p50
~1055 → ~35 ms. Yük ölçümlerinde tick yolunu
etkilemiyor (katılma pencerenin ilk saniyesinde bitiyor), ama
connect_ms ve kısalan oturum penceresi WS koşularını TCP'ninkinden ~1 sn
geç başlatabiliyor.

## 6. Taşıma soyutlaması (TCP + rUDP)

```rust
trait Transport: Send + 'static {
    fn bind(self: Arc<Self>, addr) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>>;
}
trait Listener: Send + Sync + 'static {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>>;
    fn local_addr(&self) -> Option<SocketAddr> { None }
    fn close(&self) {}   // §9: bekleyen accept'i bitirir (B16); rUDP demux'ı da
    fn handshake_stats(&self) -> Option<HandshakeStats> { None }  // B31: WS/TLS/QUIC
}
struct Endpoint { /* pump görevlerini başlatan tek FnOnce; mailbox'ları taşır */ }
```

`Endpoint`, bağlantı için pump görevlerini başlatır ve kaynaklarını
(socket yarısı, datagram soketi, …) tamamen kendi içinde sahiplenir.
Aktör katmanı pump'ları handle'ler dışında hiç bilmez.

**TCP** (`gsb_net::tcp`): klasik yol — bir socket, bir reader pump
(idle deadline'lı, §3), bir writer pump.

**Dinleme kuyruğu: `listen_backlog` (BACKLOG B84 — 2026-09-28).** TCP
tabanlı her dinleyen soket — TCP, TLS, WS kapıları ve ops HTTP yüzeyi —
`gsb_net::listen::bind_tcp(addr, backlog)` ile bağlanır: tokio'nun
`TcpSocket`'i, `tokio::net::TcpListener::bind`'in kurduğu soketin aynısı
(adresin ailesi, Windows dışında `SO_REUSEADDR`, `bind`, `listen`) —
tek farkı `listen(2)`'ye giden sayı. Eskiden o sayı mio'nun sabitiydi:
**128** (mio 1.1'den beri std'ninki; B50'nin okuması ve B84 satırı
1024 yazıyordu — o, mio 1.1 öncesinin değeriydi). Kararlar: (1)
*Varsayılan 128* (`DEFAULT_LISTEN_BACKLOG`) — bugünkü kuyruk birebir;
yazılmamış bir config'in kapısı değişmez, sayı artık bir bağımlılık
yükseltmesiyle sessizce değişemez. 1024'e çekmek gözlenir bir değişiklik
olurdu (taşma sayısı, connect p99) — "motor, oyun değil": büyüğü
operatör ister. (2) *Tek sunucu anahtarı* (`Config::listen_backlog`,
OPS §2); taşıma yapılarında alan (`TcpTransport`, `WsTransport`,
`TlsTransportConfig` → `listen_backlog`), `bind_listener` ve ops
kapısı (`boot/start/ops_door.rs`) config'inkini verir. (3) *Aralık*
`1..=i32::MAX` (C `int`); `0` hem başlatmada (`BadListenBacklog`) hem
kurucuda (`InvalidInput`) reddedilir; çekirdek `min(değer, somaxconn)`
uygular — tavanı aşmak hata değil. (4) *UDP kapıları kuyruksuz:* rUDP ve
QUIC'in fırtına karşılığı soketin alma arabelleği (`SO_RCVBUF`, B4) —
ayrı düğme (B4'te geldi, aşağıda). Yeni bağımlılık yok (`socket2` yerine
tokio'nun `TcpSocket`'i; `socket2` zaten `gsb-server`'ın doğrudan
bağımlılığı, `gsb-net`'e eklenmedi). Ölçüm: RPC-CONTROL-PLANE §8.2
"B84"; loadgen `--listen-backlog N` (in-process ve `--serve` sunucusu;
orkestratör sunucu çocuğuna iletir).

*Elenenler.* (a) *Kapı başına değer* (`[[listeners]]` girdisinde alan) —
ölçülmüş bir ihtiyaç yok; kapıların hepsi aynı katılma patlamasını
paylaşır (girdiye yazılmış `listen_backlog` F61'den beri başlatmayı
durdurur — aşağıda; önceden sessizce etkisizdi); gerekirse girdiye
isteğe bağlı alan olarak geriye uyumlu eklenir. (b) *`socket2`
ile kurmak* — `gsb-net`'e yeni doğrudan bağımlılık; tokio'nun
`TcpSocket::listen(backlog)`'u aynı sistem çağrısını güvenli API'yle
yapıyor. (c) *Varsayılanı `somaxconn`'a çekmek* (`listen(-1)` ya da
`i32::MAX`) — her kurulumun kuyruğunu değiştirir ve SECURITY §4.4'ün
"patlamaya göre boyutla" ilkesini tersine çevirir. (d) *Yalnız
loadgen'de düzeltmek* — ölçüm düzeneği motorun bir yeteneği olmadan
kuyruğu değiştiremez; gerçek bir oyunun ani katılma yükü de aynı düğmeyi
ister.

**UDP kapılarının soket arabellekleri: `udp_recv_buffer_bytes` /
`udp_send_buffer_bytes` (BACKLOG B4 — 2026-09-28).** UDP kapısının
accept kuyruğu yok: rUDP'de TEK soket (demux) ve QUIC'te uç noktanın
TEK soketi o kapının bütün oturumlarını taşır; patlama altında tek
tampon çekirdeğin alma kuyruğudur ve dolu kuyruğa gelen datagram'ı
çekirdek, sunucu görmeden düşürür (Linux `/proc/net/snmp`
`Udp: RcvbufErrors`) — gsb'nin hiçbir sayacı bu kaybı göremez. İki
kapının soketi artık `gsb_net::listen::bind_udp(addr, UdpBuffers)` ile
kurulur (`socket2`: adresin ailesi, close-on-exec, bloklamayan; arabellek
boyutları `bind`'dan ÖNCE uygulanır, hiçbir datagram küçük kuyrukta
beklemez); rUDP `tokio::net::UdpSocket::from_std`, QUIC
`quinn::Endpoint::new(EndpointConfig::default(), …, default_runtime())`
— `Endpoint::server`'ın kurduğunun aynısı, tek farkı soket. Kararlar:
(1) *Yazılmazsa dokunulmaz* — `setsockopt` hiç çağrılmaz, sistem
varsayılanı (Linux `net.core.rmem_default`/`wmem_default`, çoğu dağıtımda
212 992 B) kalır: anahtardan önce her UDP kapısının sahip olduğu soket
(test: ayarsız soket düz `std` bind'ınkiyle aynı boyutları okur).
(2) *İki sunucu anahtarı, her iki yön* (`Config::udp_recv_buffer_bytes`,
`udp_send_buffer_bytes`, OPS §2); taşıma yapılarında `buffers:
UdpBuffers` alanı (`UdpTransportConfig`, `QuicTransportConfig`),
`bind_listener` config'inkini verir. `SO_SNDBUF` aynı yoldan, bedelsiz
geldi: dolu gönderme kuyruğu rUDP'de bant bant sayılıyor
(`udp_*_datagrams_send_failed`). (3) *Aralık* `4096..=i32::MAX` (bir
sayfadan C `int`'e); dışı hem başlatmada (`BadUdpBuffer { key, value }`)
hem kurucuda (`InvalidInput`) reddedilir. 4096'nın altı reddedilir,
çünkü Linux onu sessizce kendi tabanına (~2,3 KiB) yükseltir — operatörün
kastettiği asla o değildir. (4) *Çekirdeğin kuralı:* Linux isteği
`net.core.rmem_max`/`wmem_max`'ta keser, sonra **ikiye katlar** (ikinci
yarı çekirdeğin muhasebesi; `getsockopt` iki katını okur) — kullanılabilir
alan tavana kadar yaklaşık istenen kadardır. Kesilen istek hata değil
(`setsockopt` de hata vermez); kapı bind'da verilen boyutları log'lar ve
verilen yarı istenenin altındaysa sysctl'ü adlandıran bir uyarı basar
(`listen::log_buffers`). macOS `kern.ipc.maxsockbuf`'ta keser, katlamaz.
(5) *Yeni doğrudan bağımlılık:* `socket2` 0.6 (lock'taki 0.6.5, tokio
zaten çekiyor) artık `gsb-net`'in de doğrudan bağımlılığı — tokio'nun
`UdpSocket`'i (1.53) `SO_RCVBUF` ayarlayıcısı sunmuyor, `unsafe` yasak
ve ham fd'ye inmenin güvenli yolu bu. Ölçüm: loadgen
`--udp-recv-buffer N` (in-process ve `--serve` sunucusu; orkestratör
sunucu çocuğuna iletir); kanıt rUDP 1000 katılma fırtınasında
`/proc/net/snmp` `RcvbufErrors` farkı.

*Elenenler.* (a) *Kapı başına değer* — `listen_backlog` ile aynı gerekçe
(ölçülmüş ihtiyaç yok; girdinin grameri kapalı kalır). (b) *Varsayılanı
büyütmek* — her kurulumun soketini değiştirir ve çoğu Linux'ta
`rmem_max` (212 992) zaten keser: sessiz bir "büyüttük" yanılgısı olurdu.
(c) *`SO_RCVBUFFORCE`* (tavanı aşan, `CAP_NET_ADMIN` isteyen) — sunucu
ayrıcalık istememeli; tavan operatörün sysctl'üdür. (d) *Arabelleği
büyütmeyi el sıkışma kaybının çözümü saymak* — H turunda elendi (eşiği
taşır, kaldırmaz; aşağıda "El sıkışma kaybı"): bu bir verim düğmesi.

*Ölçüm (2026-09-28, release).* B84'ün komutu rUDP'de:
`gsb-loadgen 1000 --orchestrate --procs 2 --visibility spatial
--duration 8 --transport udp`, çocuklar varsayılan worker'larla; kayıp
`/proc/net/snmp` `Udp: RcvbufErrors`'ın koşu boyunca farkı (sistem
geneli); iki yapılandırma sırayla, üç tur, her koşudan önce yükün 1 dk
ortalaması < 5 beklendi (kabul edilen koşularda 1,5–3,9; makinede başka
bir ajan derliyordu). Makinenin `rmem_max`'ı 4 MiB; ikili `09b9254`
(B4, B2'den önce).

| Yapılandırma | etkin alma arabelleği (`getsockopt`) | `RcvbufErrors` | `hs_retries` | connect p50 / p99 ms | istemci `retrans_out` |
|---|---|---|---|---|---|
| varsayılan (dokunulmaz) | 212 992 B | 4693 · 4195 · 3370 | 1612 · 1762 · 1298 | 70/206 · 84/256 · 61/196 | 1903 · 1795 · 1323 |
| `--udp-recv-buffer 4194304` | 8 388 608 B (2×) | **0 · 0 · 0** | **0 · 0 · 0** | **24/54 · 33/67 · 46/87** | **0 · 0 · 0** |

Her koşuda `connected = joined = left = 1000`, `server_closes = 0`,
`snap_total` 228 898–235 392 (iki kolda aynı bant); `errors` varsayılan
kolun 2. turunda 2, gerisinde 0. *Okuma:* 1000 istemcilik fırtına
208 KiB'lik varsayılan kuyruğu koşu başına 3,4–4,7 bin datagram
taşırıyor — el sıkışma adımları ve ilk kontrol kareleri (istemci
`hs_retries`, `retrans_out`) — ve bu kaybın hiçbiri sunucunun
sayaçlarında görünmüyor (demux onu hiç görmez; BACKLOG B85). 4 MiB'lik
kuyrukta sıfır kayıp, sıfır yeniden gönderim, connect p99 ~3–4× kısa.

**Kapı girdisinin grameri kapalı (BACKLOG F61 — 2026-09-28).** Bir
`[[listeners]]` girdisi (`ListenerEntry`) tam dört anahtar alır —
`transport`, `bind`, `tls_cert`, `tls_key`; başka her anahtar (yazım
hatası, kapıya yazılmış sunucu anahtarı) ayrıştırmayı durdurur, hata
anahtarı, girdinin anahtarlarını ve satırı adlandırır. Girdi düz bir
struct: taşımaya özgü alt tablo ya da `flatten`/`tag`'li parça yok
(TLS dosyaları girdinin iki alanı; WS/QUIC/rUDP düğmeleri sunucu
düzeyinde), bu yüzden serde'nin `deny_unknown_fields`'ı girdinin
tamamını kapsar — `flatten` ile birleşince bozulan serde davranışı
burada yok. Girdiye ileride taşımaya özgü bir alt tablo gelirse o da
kendi `deny_unknown_fields`'lı struct'ı olur, `flatten` değil. Eskiden
anahtar sessizce atılıyordu. Taramanın tablosu: OPS §2 "Kapı girdisi".

**Config'in üst düzeyi de kapalı: motorun ve oyunların anahtarları
(BACKLOG F62 — 2026-09-28).** Üst düzey barındırılan oyunla paylaşılan
ad alanı olduğu için `Config` bilinmeyen alanı ayrıştırırken reddetmez;
onun yerine sunucu her başlatmanın İLK adımında (`start_inner`, bir şey
bağlanmadan) `Config::check_top_level_keys`'i koşar: `raw`'daki her üst
düzey anahtar ya motorundur (`Config`'in alanları — liste struct'ın
kendi türetilmiş `Deserialize`'ından okunur, kayamaz; demo'nun düz
anahtarları hariç) ya da bir oyunun `GameModule::owned_keys`'indedir —
barındırılan oyunun ya da bu ikiliye derlenmiş başka bir oyunun. Gerisi
`ServerError::UnknownKey` (anahtar, dosyadaki yazımı, benzediği bilinen
anahtar). Ayrıntı ve karar gerekçesi: OPS §2 "Üst düzey",
GAME-MODULE §4.3.

**El sıkışan kapılar: el sıkışma accept döngüsünün dışında (BACKLOG
B31 — 2026-09-26).** WS, TLS ve QUIC kapısında bir bağlantı, oturum
olmadan önce el sıkışır. Eskiden bu `accept()`'in İÇİNDEYDİ ve
sunucunun accept döngüsü (`boot/accept.rs`) her accept'i sırayla
bekler: el sıkışmalar seriydi (B29 bulgusu, §5.7 — sessiz tek soket
kapıyı 10 sn kilitliyordu; SECURITY §4.3 bunun DoS yüzü).

*İzlenen accept yolları.*

| Kapı | Önce (`accept` = tek future) | Sonra |
|---|---|---|
| TCP | `Door::admit(TcpListener::accept)` → uç nokta | değişmedi (el sıkışma yok) |
| rUDP | demux el sıkışmayı yapar, oturumu crossbeam kanalına koyar; `accept` = bloklayan havuzda `recv` | değişmedi (el sıkışma zaten tek demux görevinde, `accept`'in dışında) |
| WS | `admit(accept + set_nodelay + timeout(10 sn, perform_upgrade))`; hata → accept hatası → döngü 100 ms geri çekilir | kabul görevi: `admit(TcpListener::accept)` → yuva → el sıkışma görevi `admit(timeout(10 sn, nodelay + upgrade))`; `accept` = `admit(kuyruk)` |
| TLS | `admit(accept + timeout(10 sn, rustls accept))`; aynı hata yolu | aynı şekil, görev `TlsAcceptor::accept` |
| QUIC | `admit(endpoint.accept → timeout(10 sn, incoming.await + accept_bi))` — quinn el sıkışmayı kendi sürücüsünde yürütür ama `accept` onu ve bi-stream'i BEKLİYORDU: aynı seri yapı | kabul görevi: `admit(endpoint.accept)` → yuva → görev `admit(timeout(10 sn, incoming.await + accept_bi))`; sınır üstünde `incoming.refuse()` |

*Şekil* (`gsb_net::transport::intake`, üç kapının ortak parçası):

```text
kabul görevi ──ham accept──▶ yuva? ──evet──▶ el sıkışma görevi: TEK await,
 (kapı başına)                 │               kapı ⊃ süre sınırı ⊃ el sıkışma
                              hayır                    │ Ok
                               ▼                       ▼
                     ret (kapat) + sayaç       kuyruk (uç nokta + yuvası)
                                                       │
sunucu accept döngüsü ◀── Listener::accept ◀───────────┘ (yuva bırakılır)
```

Kurallar: (1) accept döngüsü yine TEK şey bekler (`accept`, kuyruğu);
kabul görevi ham accept'i; her el sıkışma görevi tek bir future'ı
(kapı ⊃ `timeout` ⊃ el sıkışma — pump deyimi). Hiçbir yerde select
yok. (2) Yuva ham accept'ten accept döngüsünün uç noktayı almasına
dek tutulur (`in_flight` = el sıkışan + bitip alınmayı bekleyen); sınır
üstündeki bağlantı hemen kapatılır (QUIC: `refuse`) ve sayılır —
bekletilmez. (3) Kuyruk crossbeam'in sınırsız türü + tokio `Notify`:
uzunluğu yuvalarla sınırlı (her girdi bir yuva taşır); alıcı `&self`'ten
`try_recv` eder (kilitsiz, rUDP'nin crossbeam kararı), bekleme `Notify`
ile (`notify_one` bekleyen yokken uyanmayı saklar — kayıp uyanma yok);
bloklayan havuzda park eden iplik yok. (4) Her görev dinleyicinin
`Door`'u altında: `close()` ham accept'i, uçuştaki el sıkışmaları ve
bekleyen `accept`'i keser, kuyruğu boşaltır; son dinleyici tutamacının
düşmesi de kapatır (`IntakeHandle` — soket artık tutamaçta değil kabul
görevinde; eskiden tutamacı düşürmek soketi kapatırdı). (5) Ham accept
hatası (EMFILE) kabul görevinde 100 ms geri çekilmeyle karşılanır —
el sıkışan kapılarda sunucu döngüsü artık yalnız kapanış hatasını görür
(geri çekilme kolu TCP/rUDP'ye kaldı). Başarısız/süresi dolan el sıkışma sayılır ve `warn`
basar; accept döngüsüne hiç ulaşmaz.

*Sınır (karar).* Kapı başına uçuştaki el sıkışma = sunucunun unauthed
cap'i (`max_unauth_conns`'un çözülmüş değeri; varsayılan 25 000; cap
`0` ile kapalıysa türetilmiş varsayılan) — `boot/start/pre_auth.rs`
`handshake_bound_of`, `bind_listener` onu her el sıkışan kapının
`max_pending_handshakes`'ine verir. Gerekçe (SECURITY §4.3 #2): uçuştaki
el sıkışma, unauthed bağlantı olmaya giden bağlantıdır; tek pre-auth
bütçesi iki evreyi de sınırlar, el sıkışma evresi sonrakinden ucuz
tüketilemez. Motor, oyun değil: taşıma yapılarında alan (`WsTransport`,
`TlsTransportConfig`, `QuicTransportConfig`), doğrudan gömene
`DEFAULT_MAX_PENDING_HANDSHAKES` = 1024; sunucu config'ine yeni anahtar
eklenmedi.

*Sayaçlar.* `Listener::handshake_stats()` (varsayılan `None`; üç kapıda
`HandshakeStats { in_flight, completed, refused, timed_out, failed }`),
kabul görevinin kapanış özeti (`info`, "handshake intake stopped" — rUDP
demux'ının kapanış özetinin eşi), sınıra dayanan dönemin ilk reddinde
tek `warn`. Dinleyiciler için metrik yolu yok (metrik toplayıcı aktör
olaylarıyla beslenir); bir yol açmak toplayıcının dinleyicileri
yoklaması demekti — ölçülen bir ihtiyaç yokken eklenmedi.

*Elenenler.* (a) *`accept` içinde eşzamanlılık* — `accept` hem ham
accept'i hem biten el sıkışmayı beklemek zorunda kalırdı: iki kaynak,
select (lint). (b) *Sunucu döngüsünde ham soketi alıp el sıkışmayı
orada spawn etmek* — `Listener`'ı iki adıma böler, her kapının el
sıkışmasını sunucuya sızdırır. (c) *tokio mpsc kuyruk* — alıcı `&mut`
ister, `Arc<dyn Listener>` içinden kilitsiz erişilemez. (d) *crossbeam
`bounded(max)`* — dizi türü kapasiteyi baştan ayırır (25 000 × uç
nokta, kapı başına); sınırsız tür + yuva sınırı aynı üst sınırı
kullanım kadar bellekle verir. (e) *Sınır üstünde bekletmek* —
sınırsız kuyruk ya da taşan backlog; ret ucuz ve görünür. (f) *Ret
edilen WS bağlantısına HTTP 503* — ret yolunda yazmak (yavaş okuyana)
iş demek; kapatmak en ucuzu. (g) *Ayrı `max_pending_handshakes` config
anahtarı* — ikinci bir pre-auth düğmesi; cap'ten ayrı ayarlanırsa el
sıkışma evresi ucuzlayabilir. (h) *tokio `Semaphore`* — `try_acquire`
yeterdi, ama atomik sayaç + RAII yuva daha küçük ve `in_flight`
sayacının kendisi.

*Testler (önce kırmızı; mutasyonlu).* Eski kodda kırmızı:
`ws::tests::off_accept::{an_idle_peer_does_not_hold_the_door,
a_failed_upgrade_is_not_an_accept_error,
close_cuts_the_upgrades_in_flight}`, `tls::tests::off_accept`'in aynı
üçü (gerçek rustls istemcisi), `quic::tests::off_accept::
a_peer_without_its_stream_does_not_hold_the_door`,
`gsb-server/tests/handshake_door.rs` (2: sessiz eşler varken TLS el
sıkışması ve WS yükseltme + AUTH < 2 sn; `max_unauth_conns = 1` iki
kapının da sınırı — hemen EOF). Yeni API ile: `transport::intake::tests`
(8), `ws::tests::off_accept::{upgrades_run_side_by_side,
over_the_bound_a_connection_is_refused_and_counted}`,
`quic::tests::off_accept::over_the_bound_…` (`refuse`),
`boot::start::pre_auth::tests` (2). Değişen iki test:
`tls::tests::wrong_ca_fails_the_handshake` ve QUIC eşi artık accept
hatası değil `failed` sayacını bekliyor (sözleşme değişti: başarısız el
sıkışma accept'e ulaşmaz). Öldürülen mutasyonlar: kabul görevinin
el sıkışmayı beklemesi (seri; WS/TLS ve QUIC ayrı ayrı), sınır kontrolü
yok, ret sayılmıyor, sunucu sınırı iletmiyor (TLS/QUIC kolu ve WS kolu
ayrı ayrı), süre sınırı yok, el sıkışma kapı altında değil, tutamacın
düşmesi kapatmıyor, `close` kuyruğu boşaltmıyor, yuva tamamlanınca
bırakılıyor, QUIC `refuse` yerine `ignore`.

*Ölçüm (önce = `5ffa0b3`, sonra = bu tur; release, 32 çekirdek,
dönüşümlü önce/sonra; parantezde koşudan hemen önceki 1 dk yük
ortalaması; connect p50/p99 ms).* Sessiz soket deneyi (B29'unki):
`gsb-loadgen --serve --transport ws|tcp --bind 127.0.0.1:P --duration 30
--write-stall-secs 0`, hiç bayt göndermeyen TEK bir TCP bağlantısı,
0,5 sn sonra `gsb-loadgen 20 --addr 127.0.0.1:P --transport ws|tcp
--duration 3`. Orkestre: `gsb-loadgen --orchestrate 500 --procs 2
--transport ws|tcp --write-stall-secs 0 --duration 20`.

| Deney | Kapı | Önce | Sonra |
|---|---|---|---|
| sessiz soket + 20 istemci | WS | 9612/9612 (1,00) · 9612/9612 (0,69) · 9611/9611 (0,66); joined 0 (bağlanma 3 sn'lik koşudan uzun) | **0/0** (0,67) · 0/0 (0,92) · 0/0 (0,86); joined 20 |
| sessiz soket + 20 istemci | TCP | 0/0 (1,05) · 0/0 (1,56) | 0/0 (1,20) · 0/0 (1,41) |
| orkestre 500 | WS | 1057/1457 (1,51) · 1052/1457 (1,37); dropped 10 · 29 | **34/1075** (1,24) · 36/1075 (1,29); dropped 0 · 0 |
| orkestre 500 | TCP | 21/1014 (2,30) · 16/1017 (1,57); dropped 116 · 116 | 18/1054 (1,78) · 15/1053 (1,12); dropped 123 · 116 |
| orkestre 500, varsayılan worker'lar (B50, `73da266`) | WS | — | 29/1064 (3,46) · 32/1057 (4,84) · 34/1075 (4,13); dropped 0 · 0 · 0, sends_closed 0 · 0 · 0 |
| orkestre 500, varsayılan worker'lar (B50, `73da266`) | TCP | — | 33/1066 (3,73) · 31/1072 (4,93) · 28/1042 (4,95); dropped 0 · 0 · 0, sends_closed 0 · 0 · 0 |

Yukarıdaki dört "orkestre 500" satırı tek worker'lı çocuklar (B37
öncesi) koşulundadır; son iki satır aynı komutun varsayılan worker'lı
yeniden ölçümü (2026-09-28). Bağlanma sayıları değişmedi (p99 ~1,05 sn:
birkaç istemci hâlâ 1 sn'lik SYN yeniden gönderimini yiyor — varsayılan
worker'lı istemciler fırtınayı sertleştiriyor, dinleme kuyruğu koşu
başına 257–411 kez taşıyor; BACKLOG B84); TCP'nin 116/123'ü tek
worker'ın zamanlamasıydı (RPC-CONTROL-PLANE §8.2 "B50").

Her orkestre koşuda connected = joined = left = 500, errors =
server_closes = 0. *Okuma.* Sessiz soket kapıyı artık tutmuyor: WS
TCP'nin 0 ms'sinde. Orkestre 500'de WS p50 ~1055 → ~35 ms (backlog
taşması bitti); p99 ~1,07 sn artık TCP'ninkiyle aynı bantta (1,01-1,05
sn — iki kapıda da birkaç istemci 1 sn'lik SYN yeniden gönderimini
yiyor). WS p50'nin TCP'ye (15-21 ms) göre +15-20 ms'si yükseltmenin kendi
gidiş-dönüşü. TCP gürültü içinde değişmedi. WS `dropped` 10 · 29 → 0 · 0
(B32'nin katılma fırtınası sayacı; iki çiftle nedensellik iddia
edilmiyor). TLS kapısı ölçülmedi — loadgen TLS sunucusu kuramıyor
(§5.7); onun düzeltmesini `tls::tests::off_accept` ve
`handshake_door.rs` gerçek rustls istemcisiyle kilitler.


**rUDP** (`gsb_net::udp`, bu tur): bir UDP soketi **tüm** oturumlar için
ortaktır, yani "bağlantı" bir socket değil, datagram akımlarından
**sentezlenen** bir mantıksal oturumdur. Topoloji:

```text
tek demux görevi (listener'ın, bind'de başlatılır)
  ├─ soket READ'i (tek beklenen kaynak: recv_from; deadline heap varsa
  │  timeout(min_deadline, recv_from) — deadline yalnız read beklerken
  │  tetiklenir; hazır datagram her zaman kazanır)
  ├─ her datagramı peer adresiyle oturum tablolarına route eder
  │  (HashMap<SocketAddr, UdpSession>; oturum = inbox mailbox + ACK/
  │  sıralama durumu + last_seen + idle deadline)
  └─ decode edilen frame'i oturumun inbox mailbox'ına try_send eder
her oturum: tek WRITER görevi (reader yok — demux ortak reader)
```

`accept()` bir datagram değil, **oturum** döndürür: oturum, cookie
el sıkışması tamamlandığı anda demux tarafından ön-oluşturulur (endpoint
kanalına; crossbeam `Receiver`'in `&self`'den çalışması — kilit yasağı
altında `Arc<dyn Listener>`'den akışın tek yolu; bkz. ROADMAP tur
notu) ve accept loop, sunucunun geri kalanı için hiç değişmeden
işlenir. **Aktör katmanında değişiklik sıfır** kaldı; `gsb-core`'e
dokunulmadı. Sızan şey `gsb-net` trait şekli (nedeni ROADMAP'te):
(1) `Endpoint::take_inbox`/`take_outbox` — demux, oturum mailbox'larını
el sıkışmada, accept loop çalışmadan **önce** kurmak zorunda;
(2) `start_pump`/`PumpSpawner` artık `(Option<JoinHandle>, JoinHandle)`
döndürür — UDP'de per-connection reader yoktur; (3) `Listener::close`
(§9'ların ertelenmiş kapısı) — demux görevi tüm bağlantılardan uzun
yaşar; (4) peer adresi endpoint'ten aktöre taşınır (ihlal kapatma
sinyali; §14.5).

**El sıkışma (stateless cookie):** `istemci→HELLO{nonce,0}` /
`sunucu→HELLO{nonce,F(nonce,peer,key,slot)}` /
`istemci→HELLO{nonce,cookie}` → oturum kurulur → `sunucu→ACK{1}`
(kabul; aşağıda "El sıkışma kaybı"). `F` = splitmix64
katlanması. **Cookie key** proses başına 16 bayttır;
iki kaynaktan biriyle kurulur: (1) konfigürasyonda `cookie_key`
verilmişse o (operatör denetimi — deploy'da sabit anahtar isteyenler
için), (2) verilmediyse **OS entropisinden** (`getrandom`) 16 bayt.
Sessiz zayıf geri düşüş **yoktur**: entropi kaynağı başarısız olursa
süreç başlatmayı reddeder (yapı `Result` döndürür; duvar saati gibi
tahmin edilebilir bir değer asla kullanılmaz). Düz metin kapıda
kriptografik katman (HMAC/imza) yok — mühürlü kapıda (B5a, varsayılan)
Noise NK bu el sıkışmaya biner, aşağıda "Kayıt katmanı"; ama key tahmin
edilemez, dolayısıyla sahte-proof koruması key'in *gibi görünen*
(tahmin edilemez) olmasına dayanır. Çift yönlü mesajlar
aynı boyutta → amplifikasyon oranı ≤ 1; sahte proof, key bilmeden
üretilemez. Kabul (5 B) yalnız doğrulanan proof'a gider: oran 5/18,
sahte proof'a hiçbir şey.

**Kaynak başına bekleyen oturum sınırı (B89 — u89, 2026-10-02;
opt-in).** Çerez alışverişi durum tutmaz; durum doğrulanan proof'la
başlar (B5a'dan sonra DH de orada). `max_handshakes_per_source` (D11'in
anahtarı, aynı anlam: bir kaynağın bir kapıda ilk durumundan accept
döngüsünün uç noktayı almasına dek tuttuğu) rUDP kapısında bir kaynağın
**bekleyen** oturumlarını sınırlar — demux'ın kurduğu, accept döngüsünün
henüz almadığı. Varsayılan yok (kapı bayt bayt eskisi). Sınırdaki
kaynağın doğrulanan proof'u hiçbir şey kurmaz, kabul almaz, sayılır
(`udp_proofs_refused_per_source`); istemci proof'u yeniden yollar, yer
açılınca girer. Bakış çerezden **sonra**: yol dışı sahteci çerezi
geçemez, dolayısıyla yalnız dönüş yolu olan kaynak sayılır; kurbanın
adresiyle sahte proof ya da challenge isteği yer tutmaz. Sayım demux
görevinde (`udp::demux::source`, kilitsiz): yer, kuyruğa giren uç
noktanın taşıdığı `Pending`'de durur, bıraktığında (accept döngüsü
aldı, dinleyicinin kuyruğuyla düştü, dolu kanalda söküldü) oturumun
anahtarını demux'a bir kuyrukla geri yollar; demux her karardan önce
toplar. Girdi yalnız bir yer tutuldukça yaşar: tablo ve kuyruk uç nokta
kanalının sınırını (1024 + bekleyen accept başına bir) aşamaz.
`ConnOpened` alımdan sonra gider: oturumu sonra D12 sayar, ikisi aynı
anda asla. Accept döngüsü uç noktayı hemen aldığı için kaynak sınırı
ancak bir an tutar — sınır oturumları değil, eşzamanlı kuruluşları
keser; DH'nin **hızını** sınırlamaz (o B5a'nın bütçesi, RUDP-SECURITY
§4/§12).

**Cookie rotasyonu (yakalanan proof'un son kullanma tarihi):** key tek
başına yetmiyordu. `F` yalnız (key, nonce, peer)'in fonksiyonu olduğu
sürece telden yakalanan bir proof **proses ömrü boyunca** geçerli
kalır — aynı görünen adresten istediği zaman tekrar oynatan biri
oturumu yeniden kurar; el sıkışmanın bütün işi olan "bu dönüş yolunun
sahibi olduğunu ŞİMDİ kanıtla" cümlesinden "şimdi" düşer. Bu yüzden
`F`'nin dördüncü terimi bir **zaman dilimi** (slot): bind'dan bu yana
geçen `COOKIE_SLOT` (10 sn) periyodunun tamsayı sayacı. Sunucu
challenge'ı GÜNCEL dilim için üretir, proof'u güncel **ya da bir
önceki** dilim için kabul eder.

- **Tel değişmedi:** hâlâ `3 HELLO [u64 nonce][u64 cookie]`, her iki
  yönde 18 bayt. Slot gönderilmez — sunucunun kendi iki hesabı da onu
  aynı saatten okur, istemcinin varlığından haberi olması gerekmez.
- **El sıkışma stateless kalır:** slot doğrulama anında bir `Instant`'tan
  yeniden hesaplanır. El-sıkışma öncesi tablo yok, timer görevi yok,
  paylaşılan rotasyon durumu yok, kilitlenecek bir şey yok.
- **Sır hâlâ key:** entropi türevli, asla saat türevli değil. Slot
  *public* bir sayaçtır ve tam da public olduğu için key'le birlikte
  katlanır. İkisi karıştırılmamalı: KEY = tahmin edilemezlik,
  SLOT = son kullanma.

**Aralık: 10 sn, yani 10-20 sn'lik tekrar-oynatma penceresi.** Proof,
üretildiği dilimden SONRAKİ dilimin sonuna kadar kullanılabilir; pencere
proof'un dilim içindeki yerine göre bir ile iki periyot arasıdır. İki
gerçek sayıya göre boyutlandı: (a) *altında*, kırmamak zorunda olduğu el
sıkışma — proof, challenge alındıktan bir RTT sonra üretilir, artı
istemci zamanlayıcısının eklediği gecikme (kötü bir hatta ~300 ms,
telsizi uykudan kalkan bir telefonda birkaç saniye); 10 sn bunun 3-30
katı, dolayısıyla meşru bir el sıkışma rotasyona asla yenilmez (yenilse
bile istemcinin kendi retry'si taze challenge alır — sunucu bunda
idempotenttir); (b) *üstünde*, açık bıraktığı maruziyet — 10-20 sn,
herhangi bir gerçekçi yakala→tekrar-oynat hattının çevrim süresinden
kısa, ve rotasyonun çalışma zamanı maliyeti el sıkışma başına iki
tamsayı bölmesi, sıfır durum.

**Elenen alternatifler:** (1) *zaman damgasını cookie bitlerine gömmek*
— 64 bitin 16'sını kaba bir üretim zamanına ayırıp tek dilim doğrulamak;
aynı politikayı ifade eder ama cookie'nin dayandığı tek şeyi, sahte
üretilemez değerin genişliğini, 64 bitten 48'e indirir; (2) *üretilen
cookie'leri bir kümede tutmak* — tam tek-kullanımlık semantik, ama
stateless el sıkışmanın var olma sebebi olan "doğrulanmamış peer başına
tahsis yok" kuralını çiğner (saldırganın sahte challenge istekleri
tabloyu boyutlandırır); (3) *key'i döndürmek* — gözlemlenebilir davranış
aynı, ama sabitin yerine mutable bir sır koyar (aynı anda iki key yaşar,
ikisi de demux görevinden yazılır ve `bind`'ın "entropi ya da başlama"
garantisi artık çalışma zamanındaki her yeni çekiliş için de geçerli
olmak zorundadır). Slot terimi aynı son-kullanma'yı key'i değişmez
bırakarak sağlar — bir kez, bind'da çekilir, testin kilitlediği gibi.

**El sıkışma kaybı (H turu — `connect` kabulü bekler).** Eskiden
istemci challenge isteğini yeniliyordu (500 ms, 3 sn) ama proof'u
GÖNDERDİĞİ an kendini bağlı sayıyordu. Proof kaybolursa (ölçüldü: aynı
anda 200+ el sıkışma loopback'te tek sunucu soketinin alım kuyruğunu
taşırıyor) sunucuda oturum yoktu, AUTH boşa düşüyor, REL bandı 5 sn
sonra ölüyordu: afe7fba'da kademesiz 200 istemcinin 62-101'i katılıyordu.
Kayıp noktaları: challenge isteği/challenge → önceden de yenileniyordu;
**proof → hiç iyileşmiyordu**; ilk kontrol karesi (AUTH) → REL
yeniden gönderimi (değişmedi).

- **Karar: istemci yalnız sunucunun sözüyle bağlı.** Doğrulanan (ve
  endpoint'i accept loop'a ulaşan) proof'a sunucu **kabul** yollar:
  `ACK{1}` — yeni oturumun kümülatif ACK'i, "bana seq 1'i yolla"; var
  olan datagram türü, eski istemci için etkisiz. `UdpClient::connect`
  ancak sunucu oturumu tuttuğunu gösterince döner: kabul ya da HERHANGİ
  bir oturum datagram'ı (ACK/REL/RAW/FRAG — sunucu bunları yalnız oturum
  tablosundaki peer'e yollar; kabulün yerine gelen kare kaybolmaz, normal
  giriş yoluna verilir). O zamana kadar güncel adım (challenge isteği ya
  da proof) adımın zamanlayıcısı dolunca (B2'den beri REL bandının
  zamanlayıcısı: 50 ms'den başlar, her yeniden gönderimde ikiye katlanır;
  B86'dan beri el sıkışmada en çok **200 ms** — aşağıda "Yeniden gönderim
  zamanlayıcısı" ve "El sıkışma geri çekilmesinin tavanı"; B2'ye kadar
  sabit 50 ms, `HANDSHAKE_RTO`)
  yeniden gönderilir; `HANDSHAKE_DEADLINE`'da (= REL canlılık sınırı,
  5 sn) `TimedOut` ile vazgeçilir. Sayaçlar: istemci
  `challenge_retries`/`proof_retries`, loadgen `hs_retries`, sunucu
  demux `proofs_reanswered`. RTO seçimi ölçüldü: 500 eşzamanlı el
  sıkışmada 250 ms'ye karşı 50 ms connect p50'yi ~250 ms'den ~50 ms'ye
  indirdi, yeniden gönderim sayısı artmadı (sürü etkisi yok).
- **Sunucu proof'ta idempotent.** Oturumu olan adresten gelen geçerli
  proof bir YENİDEN gönderimdir (ilk kabul ya da proof'un ilk kopyası
  kayboldu): oturumun GÜNCEL kümülatif ACK'iyle cevaplanır, başka hiçbir
  şey olmaz — ikinci oturum yok, ikinci `ConnectionId` yok, güvenilir
  durum sıfırlanmaz. O adresten challenge isteği ya da doğrulanmayan
  proof yine cevapsız: kurulu oturum yansıtıcı olmaz.
- **Rotasyon argümanı.** Her proof yeniden gönderimi İLK cookie'yi
  kullanır (ikinci challenge yok sayılır). Cookie, istemcinin ilk
  isteğinden sonra bir N diliminde üretildi ve N+1'in sonuna kadar —
  en az bir `COOKIE_SLOT` (10 sn) — geçerli; son yeniden gönderim ilk
  istekten en fazla 5 sn sonra çıkar, dolayısıyla tek yön gecikmesi
  kalan 5 sn'nin altındaki her yolda pencereye düşer: rotasyonu aşan
  yeniden gönderim doğrulanır, yeniden başlatma yolu gerekmez. Eşitsizlik
  derleme zamanı `assert`'i: sınırı bir dilimin ötesine uzatmak el
  sıkışmayı değil derlemeyi kırar.
- **Bedel (pinlendi):** olaysız el sıkışma bir datagram uzun — HELLO
  18 B → challenge 18 B → proof 18 B → **kabul 5 B** — ve `connect` bir
  RTT geç döner (AUTH kabulü bekler). QUIC Retry ve DTLS
  HelloVerifyRequest'in, benzediği iki stateless-cookie el sıkışmasının,
  2-RTT şekli. Vazgeçiş zombi bırakmaz: inmeyen proof hiçbir şey
  ayırmadı; bütün kabulleri kaybolan oturum bir daha trafik görmez ve
  idle süpürmesi onu aktörün posta kutusu üzerinden bitirir.

**Elenen alternatifler:** (1) *`SO_RCVBUF`'ı büyütmek* (BACKLOG B4) —
eşiği taşır, kaldırmaz: daha derin kuyruk 500 el sıkışmayı yutar,
5 000'i yutmaz; kayıplı gerçek yol, sunucunun tamponu ne olursa olsun
proof düşürür. Tek kayıp datagram'ı iyileştiremeyen el sıkışma her kuyruk
derinliğinde yanlıştır; B4 bir verim ayarıdır (2026-09-28'de geldi:
`udp_recv_buffer_bytes`, yukarıda "UDP kapılarının soket arabellekleri"),
bu düzeltme değil. (2) *Sunucu tarafında el sıkışma hızlandırma*
(tick başına N kabul, gerisini düşür/ertele) — çekirdeğin demux görmeden
düşürdüğünü sunucu hızlandıramaz; ertelemek doğrulanmamış peer için
durum tutmak demek, stateless el sıkışmanın yasakladığı tam şey.
(3) *Kabul olmadan ilk sunucu datagram'ında onaylamak* (tel değişmez) —
sunucu istemcinin ilk kontrol karesine kadar hiçbir şey yollamaz;
`connect` onaysız oturum döndürür, proof'un yeniden gönderimleri REL
bandının saatine biner, sessiz kalan istemci bağlı olup olmadığını
hiç öğrenemez. (4) *İstemci karelerinin cookie taşıması* (TCP SYN
cookie tarzı, her kare yeniden doğrular) — oturum başına bir 5 B
datagram'dan kaçmak için oturumun her datagram'ına 8 B.

NAT yeniden bağlanması: `udp_migration` kapalıyken (varsayılan) yeni
4-tuple = yeni el sıkışma = yeni `ConnectionId` (eski oturum, boşta
kalana kadar idle sweep'e kadar yaşar — sınır: `idle_timeout`); açıkken
oturum CID ile yeni adrese göçer (aşağıda "Bağlantı göçü", B3). Aktörü
ölmüş oturum ise artık hemen gider — aşağıda "Aktörü ölmüş oturum".

**Datagram çerçevesi:** `[u8 kind]` — `0` RAW `[u16 op][payload]`
(oyun bandı: kayıp toleranslı, sırasız — snapshot'lar ve MOVE_TO);
`1` REL `[u32 seq][u16 op][payload]` (kontrol bandı: cumulative ACK +
RTO yeniden gönderim, **sıralı teslim** — AUTH/JOIN/LEAVE/HEARTBEAT);
`2` ACK `[u32 next_expected]`; `3` HELLO `[u64 nonce][u64 cookie]`;
`4` FRAG `[u16 mesaj id][u8 index][u8 count][parça]` (yalnız sunucu →
istemci, bütçeyi aşan oyun bandı karesi — aşağıda "MTU"); `5` PROBE
`[u32 sonda id][u32 yankı µs]` (yalnız sunucu → istemci) ve `6` REPORT
`[u32 sonda id][u32 alınan oyun datagram'ı]` (yalnız istemci → sunucu) —
oyun bandının geri bildirimi, aşağıda "Oyun bandı geri bildirimi";
`7` PATH_CHALLENGE `[u64 nonce]` (yalnız sunucu → istemci) ve `8`
PATH_RESPONSE (yalnız istemci → sunucu, her zaman etiketli) ile
`0x80 | k` CID etiketli istemci datagram'ı — bağlantı göçü, aşağıda
"Bağlantı göçü".
Band ayrımı: `op 1..=64` (11 hariç) = kontrol (güvenilir), `op ≥ 1000`
= oyun (kayıp toleranslı). Yeniden gönderim: uyarlanan RTO (RFC 6298,
50 ms–1 sn, geri çekilmeli — B2, aşağıda; önceden sabit 50 ms); tek
frame asla bırakılmaz, 5 sn ACK ilerlemesi olmazsa BANT ölü ilan edilir
ve oturum biter (`udp/mod.rs` "The REL liveness bound"; eski "250 ms'de
vazgeç" kuralı kaldırıldı), out-of-order penceresi 16; çift frame ACK'lenir ama
yeniden iletmez. ACK'ler demux tarafından oturumun writer'ına **out
kanalı üzerinden** (UDP_ACK frame olarak) verilir — komut kanalı yok,
tek-beklenen-kaynak özdeşliği korunur.

**İstemci tarafında oturumun sonu (BACKLOG B128 — 2026-10-03).** UDP'de
FIN yok: istemcinin oturumu, istemcinin kendi hükmüyle biter — üç
nedenden biriyle (`gsb_net::udp::UdpEnd`, ilk neden kalır, oturum bir
kez biter): `RelDead` (REL bandı öldü: 5 sn ACK ilerlemesi yok ya da
yeniden gönderim kuyruğu sınırı), `Reset` (oturumun jetonlu stateless
reset'i, B5b) ve `SealLimit` (kayıt sayacı tükendi `seal_exhausted` ya
da sahte kayıt bütünlük sınırı aşıldı `seal_integrity_limit`). Bitişten
sonra oturum, EOF'undan sonraki bir akış gibidir — TCP'nin anlamı
referanstır:

- **Önce alınmış kareler boşaltılır:** sıraya girmiş kontrol kareleri
  (ve elde bekleyen oyun bandı karesi) `recv_frame`'den eskisi gibi
  döner; bitişten önce gelen kaybolmaz.
- **Sonra bitiş, hemen:** `recv_frame` penceresini beklemeden ve soketi
  bir daha okumadan `Ok(None)` döner, `is_established()` `false`'tur;
  `gsb_client::Conn::recv` bunu `Recv::Closed` yapar (her sonraki okuma
  yine `Closed`). Bitişten sonra gelen datagram hiçbir oturuma ait
  değildir. Boş dönüş runtime'ın işbirlikli bütçesinden geçer
  (`consume_budget`): bitişi kaçırıp okumayı sürdüren çağıran, akış
  EOF'undaki gibi işçiyi kilitlemez, verir.
- **Sonra hiçbir şey gönderilmez:** `send_frame` (ve `rebind`)
  `NotConnected` ile reddeder, hiçbir sayaca dokunmaz — bitiş, olduğu
  anda, nedeniyle bir kez sayılır. Bitişin geride bıraktığı da yanında
  sayılır: bekleyen kontrol kareleri `gave_up`, bir boşluğun arkasında
  kalmış alınmış kareler `oob_at_end` (artık sırayla teslim edilemez).

Yük üreteci bitişi akışınki gibi okur (istemci hemen durur, LEAVE
gönderemez) ve her oturumun sonunu, teli bırakılırken nedeniyle BİR
KEZ sayar: RESULT ve `CLIENT` satırlarında `udp_ends_rel_dead`,
`udp_ends_reset`, `udp_ends_seal_limit` (sıfırlar dahil hep var; churn'de
döngü başına). Kilitler: `gsb-net` `udp::client::tests::end` (dört
neden, boşaltma, anında bitiş, bitişten sonra sayılmayan gönderim),
`gsb-client` `conn::tests` (betikli düz metin sunucu, REL ölümü →
`Closed`), yük üreteci `view::run::tests::ended` (bir bitiş, bir sayım)
ve `rudp_resume.rs`'in B5b yeniden başlatma testi (`Closed`, neden
`Reset`).

**Yeniden gönderim zamanlayıcısı: RTT tahmini + uyarlanan RTO (BACKLOG
B2 — 2026-09-28).** Eskiden REL bandı en eski ACK'siz karesini her
eş için sabit 50 ms'de bir yeniden gönderiyordu, geri çekilme yoktu:
200 ms'lik yol ilk ACK gelebilmeden her kontrol karesinin ~4 fazladan
kopyasını alıyor, kesinti canlılık sınırına dek 20 Hz'de dövülüyordu.
Şimdi her yön — sunucunun oturum başına yazıcısı ve istemci — bandın
gönderen yarısında (`udp::rel::RelSend`, `rel::Rto`; yazıcı ile istemci
artık aynı kodu paylaşıyor) RFC 6298 tahmini tutar. **Tel değişmedi.**

- **Örnek nereden:** var olan kümülatif ACK'ten. Gönderen her ACK'siz
  kareyi gönderim zamanıyla zaten tutuyordu; kare serbest bırakan bir
  ACK TEK örnektir: serbest bıraktığı EN YENİ karenin `şimdi − gönderim`
  süresi (ACK'i onun varışı yollattı). Sunucuda örnek ACK'in demux →
  yazıcı kanalı yolculuğunu da içerir: yazıcının gerçekten gördüğü tur.
- **Karn kuralı:** serbest bırakılan karelerden biri bile yeniden
  gönderildiyse örnek yok (ACK hangi kopyaya cevap, bilinemez; yeniden
  gönderilen karenin doldurduğu boşluk arkasındaki her karenin ACK'ini
  geciktirir); geri çekilmiş zamanlayıcı temiz bir kare cevaplanana dek
  korunur — ya da bant zamanlayıcıdan uzun süre boşta kalana (hiçbir şey
  ACK beklemiyor) dek: o zaman sonraki kare tahminden başlar, geri
  çekilme düşer. Kontrol bandı dakikada bir kare taşır; bu kural olmadan
  katılma fırtınasının geri çekilmesi oturumun LEAVE'ini karşılıyordu
  (ölçüldü: 1000 LEAVE'in 19'u loadgen'in 500 ms'lik penceresini
  kaçırdı). Yakın aralıklı kareler geri çekilmeyi korur: RTT'si
  zamanlayıcıyı aşan yol ancak böyle temiz örnek verir (Karn).
- **Zamanlayıcı:** `SRTT + max(1 ms, 4·RTTVAR)` (α = 1/8, β = 1/4),
  `[50 ms, 1 sn]`'ye kıstırılır; her dolmada ikiye katlanır (tavana
  kadar), geçerli örnek geri çekilmeyi sıfırlar. İlk örnekten önce
  taban (50 ms).
- **Sınırlar:** *taban 50 ms* — eski sabit değer (H turunun katılma
  fırtınası ölçümünün seçtiği); ACK'i iki tarafta da zamanlanmış
  görevler üretir (demux, yazıcı kanalı, istemcinin okuma döngüsü),
  milisaniyenin altındaki LAN turu bile zamanlayıcı bekleyen ACK görür —
  60 Hz'de ~3 tick'in altı meşgul makinede hiç kaybolmamış kareleri
  yeniden yollar. *Tavan 1 sn* — kesintiden (Wi-Fi dolaşımı, hücresel
  geçiş) dönen yol en geç bir saniyede yeniden denenir ve canlılık
  sınırı (5 sn) tavanda en az dört deneme görür (derleme zamanı
  `assert`'i: `REL_NO_ACK_FATAL ≥ 4 × MAX_RTO`).
- **Canlılık sınırı değişmedi:** ölüm hâlâ "5 sn kümülatif ACK
  ilerlemesi yok" — kanalın saati; geri çekilme yalnız yeniden gönderim
  TAKVİMİNİ değiştirdi. İlk gönderimden itibaren kesintide yeniden
  gönderimler 50, 150, 350, 750, 1550, 2550, 3550, 4550 ms'de: 100 yerine
  sekiz.
- **Uyanma:** yazıcı sonraki partiyi en eski karenin zamanlayıcısı
  dolana dek bekler, ama asla `RETRANSIT_TICK`'ten (50 ms — reap
  denetimi ve metrik boşaltma aralığı, eskisi gibi) uzun değil; istemcinin
  okuması da öyle. Soketin reddettiği yeniden gönderim bir tick sonra
  aynı zamanlayıcıyla denenir.
- **El sıkışma:** aynı zamanlayıcı — her adım tabandan başlar, her
  yeniden gönderimde katlanır (B86'dan beri 200 ms'de durur — aşağıda
  "El sıkışma geri çekilmesinin tavanı"); yeniden gönderilmeden
  cevaplanan adım istemcinin ilk örneğidir, REL bandı yolun tahminiyle
  başlar (adımlar yeniden gönderildiyse örnek yok; ~~geri çekilme bandın
  başlangıcıdır~~ *B86'dan beri band geri çekilmeyi devralmaz, başlangıç
  zamanlayıcısından başlar*). Sunucu tohumlayamaz: challenge'ı durumsuz;
  ilk örneği ilk kontrol karesinin ACK'i.
- **Görünürlük:** istemci `UdpClient::srtt()`/`rto()`; yazıcının oturum
  sonu log'u `srtt_us`/`rto_ms`; sunucu geneli yeniden gönderim sayacı,
  sebebe göre: `udp_control_retransmits_timeout` (OPS §3 — bantta hızlı
  yeniden gönderim yok, her yeniden gönderim bir zamanlayıcı dolması).

*Elenenler.* (a) *Telde zaman damgası yankısı* (her REL gönderim zamanı
taşır, her ACK yankılar — TCP timestamps) — yeniden gönderilen kareden
de kesin örnek; bedeli saniyede birkaç kare taşıyan bir bant için her
REL ve ACK datagram'ında tel değişikliği. Karn kuralı bedava. (b) *RFC
6298'in 1 sn'lik başlangıç RTO'su* — ilk kontrol karesi oyuncunun
beklediği AUTH/JOIN cevabı; kaybı bir saniyelik katılma gecikmesi olurdu,
onlarca baytlık karenin gereksiz kopyası ise neredeyse bedava ve geri
çekilme uzun yolun zamanlayıcısını birkaç kopyada yukarı taşır. (c)
*Oyun bandından örnek* — RAW kareler ACK'lenmez. (d) *50 ms'nin altında
taban* — yukarıda. (e) *Yeniden deneme sayısıyla ölüm (TCP şekli)* —
canlılık sınırı turunda elendi; geri çekilme o kararı değiştirmedi.

*Ölçüm (2026-09-28, release) — bedel, dürüstçe.* Yukarıdaki B4 ölçümünün
komutu (rUDP 1000, katılma fırtınası), yük < 5; "önce" = `09b9254`,
"sonra" = `9308459` (boşta kalma kuralı dahil). Üç tur:

| Yapılandırma | `RcvbufErrors` | `hs_retries` | connect p50 / p99 ms | `errors` | istemci `retrans_out` / sunucu `udp_control_retransmits_timeout` |
|---|---|---|---|---|---|
| önce, varsayılan arabellek | 4441 (önceki koşular 3370–5345) | 1488 · 1479 · 983 | 65/216 · 56/206 · 53/149 | 4 · 3 · 0 | 2228 · 2115 · 2096 / — |
| sonra, varsayılan arabellek | 4162 (önceki koşular 3863–4502) | 1348 · 1515 · 1405 | 53/755 · 65/769 · 69/756 | 75 · 49 · 114 | 1524 · 1347 · 1618 / 1919 · 2230 · 1452 |
| sonra, `--udp-recv-buffer 4194304` | 0 | 0 · 16 | 19/51 · 21/59 | 0 · 0 | 0 · 0 / 0 · 0 |
| B86 + rapor (`29059ca`, `main` 6a00d31 üstünde), varsayılan — yük 4,9 · 4,2 · 3,2 | 4929 · 4974 · 4335 (sunucu soketi, B85: 4151 · 4196 · 3557) | 1281 · 1556 · 1364 | 55/357 · 69/557 · 65/554 | 0 · 0 · 0 | 1648 · 1890 · 1523 / 1848 · 1392 · 1170 |
| B86 + rapor (`29059ca`), `--udp-recv-buffer 4194304` — yük 4,6 · 3,5 · 2,9 | 0 · 0 · 0 (B85: 0 · 0 · 0) | 0 · 0 · 0 | 23/53 · 23/48 · 25/58 | 0 · 0 · 0 | 0 · 0 · 0 / 0 · 0 · 0 |

Her koşuda `connected = joined = left = 1000`, `server_closes = 0`,
`snap_total` 230 135–235 820. *Okuma.* (1) **Kaybolan arabellekte
bedel var:** fırtına kuyruğu taşırınca kaybolan el sıkışma adımları artık
50/100/200/400 ms'de yeniden gönderiliyor — dört adımı kaybeden istemci
~750 ms bekliyor, connect p99 ~150–220 → ~760 ms. Kaybolan AUTH/JOIN de
geri çekilmiş zamanlayıcıyla (el sıkışmanın geri çekilmesi Karn gereği
banda taşınır) daha geç yenileniyor; loadgen JOIN sonucunu beklemeden
girdi yolladığı için sunucu bu girdilere `NotInRoom` cevaplıyor
(`errors` 49–114'ün hepsi — geçici enstrümantasyonla sayıldı). Kayıp bir
tıkanıklık kaybı (tek soketin kuyruğu); geri çekilme tam da ona
verilecek TCP cevabı, ama anlık bir patlamada gecikmeyi uzatır. (2)
**Fırtınanın ilacı B4:** 4 MiB'de kayıp yok ve bütün kolların en iyi
sayıları (connect p99 51–59 ms, `errors` 0). (3) **Boşta kalma kuralı
olmadan** aynı fırtınada 1000 LEAVE'in 19'u loadgen'in 500 ms'lik
penceresini kaçırıyordu (`left = 981`; kaçıranların zamanlayıcısı 648 ms–
1 sn: fırtınanın geri çekilmesi ya da fırtına anında şişmiş bir RTTVAR
örneği); kuralla altı koşuda `left = 1000`. Şişmiş RTTVAR kuralın
kapsamı dışında: seyrek kontrol bandında sonraki örneğe dek sürer
(BACKLOG B87 — *rUDP sertleştirme 2'de kapandı: raporlayan oturumda sonda
turları bandı saniyede bir örnekler, aşağıda "Oyun bandı geri
bildirimi"*). Karar bakımcının: el sıkışma geri çekilmesine tavan ya da
bandın el sıkışmanın geri çekilmesini devralmaması (BACKLOG B86 — *karar
(a), aşağıda "El sıkışma geri çekilmesinin tavanı"*).

**Çekirdeğin soket kayıpları sunucuda (BACKLOG B85 — 2026-10-02).** B4'ün
ölçümü fırtınada koşu başına binlerce datagram'ın çekirdekte düştüğünü
yalnız sistem geneli `RcvbufErrors` farkıyla görebildi; demux o kaybı hiç
görmez. Şimdi kapının kendi soketi sayılıyor:
`udp_datagrams_dropped_kernel` (OPS §3). **Karar: soketin `/proc/net/udp`
satırı, demux'ın dışında.** Linux her sokete bir `sk_drops` tutar ve
onu `/proc/net/udp{,6}`'nın `drops` sütununda gösterir; satır soketin
inode'uyla bulunur (`/proc/self/fd/<fd>` bağlantısı `socket:[inode]`).
Bind'da, kapı metrik raporluyorsa, `udp::kernel` küçük bir görev başlatır:
tek beklenen kaynağı uyku (1 sn), sonra satırı okur ve büyümeyi
`Flusher`'la gönderir; satır kaybolunca (soket kapandı) biter, dinleyicinin
`close`'u onu demux'la birlikte keser ve `Drop`'u satırı son bir kez okur.
Sütun `u32`'dir, sarar. *Platform:* yalnız Linux; başka yerde görev
başlamaz, sayaç 0 (derleme aynı). *Bağımlılık:* yok — `std::fs`.

*Elenenler.* (a) *`SO_RXQ_OVFL`* (çekirdek her datagram'a soketin kayıp
sayısını kontrol mesajı olarak ekler) — kesin ve datagram başına, ama
seçeneği kurmak ham `setsockopt` ister: ne tokio ne `socket2` 0.6 onu
sunuyor, `unsafe` yasak, `nix` lock'ta yok; okumak da demux'ın
`recv_from`'unu kontrol tamponlu `recvmsg`'e çevirir — taşımanın en sıcak
yolunu, 1 sn'lik bir yoklamanın aynı iyi karşıladığı bir sayaç için
değiştirmek. (b) *Sistem geneli `RcvbufErrors`* — B85'in yerini aldığı
şey; makinedeki her UDP soketini karıştırır. (c) *Satırı demux'ta okumak*
— procfs okuması bütün oturumların paylaştığı tek görevde durur ve
makinedeki UDP soketi sayısıyla büyür (in-process loadgen'in 1000
istemcisi aynı tabloda).

*Testler (önce kırmızı; mutasyonlu).* `udp::kernel::tests`: sütun ve
inode ayrıştırması; sarma; okunmayan tek sayfalık bir sokette çekirdeğin
düşürdüğü tam olarak `gönderilen − kuyruktaki` (loopback'te başka kayıp
yok); kapının izleyicisi uçtan uca — tek iş parçacıklı çalışma zamanında
selin sırasında demux koşamaz, toplayıcıya giden toplam satırdakine
eşit; ilk okumadan önce kapanan kapı da (`Drop`'taki son okuma). Öldürülen
mutasyonlar: izleyiciyi başlatmamak (2 test), `Drop`'ta göndermemek (1),
farkı değil mutlak değeri toplamak (1), yanlış sütun (4). Sağ çıkan:
`close`'ta izleyiciyi kesmemek (dinleyici tutamacı yaşadıkça okumaya
devam eder; ölçen test "sessizliğe dek" türünden olurdu — yazılmadı).
**B96 kapandı (rUDP sertleştirme 3):** koşul bekleyen bir test buldu —
kapı kapanınca (dinleyici hâlâ tutulurken, soket ve satırı yaşarken)
metrik kanalının SON göndericisi kesilen izleyiciyle gider, kanal kapanır
ve son düşüşler raporlanmıştır
(`a_closed_door_stops_its_watcher`); kesmeyen mutant'ta kanal hiç
kapanmaz (10 sn sınırı yalnız başarısızlığı sınırlar). Aynı turda F75:
`udp/tests/unaccepted.rs` toplamı "300 ms sessizliğe dek" değil, son
gönderici gidene dek (kanal kapanışı) toplar; `bands.rs`/`frag.rs`/
`tests.rs`'nin olumlu okumaları 10 sn pencereli (kare gelince hemen
döner; pencere yalnız başarısızlığı sınırlar); iki olumsuz okuma ("hiçbir
şey gelmez") aynı yazıcıdan sonra yollanan bir işaretleyici kareye
çevrildi (loopback'te aynı yoldan sırayla) — daha güçlü: `bands.rs`
istemcinin aldığı oyun datagram'ını da tam sayar.

**El sıkışma geri çekilmesinin tavanı (BACKLOG B86 — 2026-10-02, bakımcı
kararı: seçenek (a)).** B2'nin ölçümü (yukarıda) iki bedel gösterdi:
kaybolan bir el sıkışma adımı 50/100/200/400 ms'de yeniden gönderiliyor,
dört adımı kaybeden istemci ~750 ms bekliyordu (varsayılan arabellekte
connect p99 ~200 → ~760 ms); ve el sıkışmanın geri çekilmesi Karn gereği
REL bandına taşınıyordu — kaybolan AUTH/JOIN de geç yenileniyordu.
Kararlar:

- **Tavan 200 ms** (`rel::HANDSHAKE_MAX_RTO`, `client::step_interval`):
  adımın zamanlayıcısı bandınki gibi katlanır ama 200 ms'de durur — 50,
  100, 200, 200, … ms. Gerekçe: adım 18 B; gereksiz bir kopya neredeyse
  bedava (46 B telde), geç kalan bir kopya oyuncuya katılma gecikmesi.
  200 ms katlamanın üçüncü adımı (B2 öncesi sabit 50 ms'nin fırtına p99'u
  ~150–220 ms'ydi), oynanabilir her yolun RTT'sinin üstünde; en kötü
  hâli — `HANDSHAKE_DEADLINE`'a (5 sn) dek her 200 ms'de bir kopya —
  saniyede beş 46 B datagram: kurulmuş tek bir oturumun oyun bandından
  az. Yolu 200 ms'den uzun bir istemci adım başına bir-iki fazladan kopya
  yollar ve o adımın cevabı örnek vermez (Karn) — bant tahminini kendi
  karelerinden kurar, sunucunun bandının hep yaptığı gibi. Tavan
  `[MIN_RTO, MAX_RTO)` içinde (derleme zamanı `assert`'i).
- **Bant geri çekilmeyi devralmaz** (`Rto::seed`): bant el sıkışmanın
  TAHMİNİYLE başlar (temiz bir adımın örneği varsa — bugünkü gibi
  SRTT'yi tohumlar), geri çekilme çarpanıyla DEĞİL. El sıkışmanın geri
  çekilmesi "bir adım kayboldu" der, "yol yavaş" demez; bant kendi
  karelerinde RFC 6298'le (Karn, katlama, boşta kalma kuralı) yine geri
  çekilir — bant içindeki davranış değişmedi.
- **Tel değişmedi;** sunucu tarafı değişmedi (challenge durumsuz, ilk
  örneği ilk kontrol karesinin ACK'i).

*Elenenler.* (b) *Olduğu gibi + `udp_recv_buffer_bytes` önerisi* —
kuyruk derinliği eşiği taşır, kaldırmaz (H turu); kayıplı gerçek yolda da
adım kaybolur. (c) *Config'e açmak* — operatörün bilemeyeceği bir sayı;
"motor, oyun değil" ilkesi bir yapı taşını açar, bir taşıma sabitini
değil. (d) *Tavanı bandın tavanına (1 sn) bırakıp yalnız devralmayı
kaldırmak* — AUTH/JOIN düzelir ama connect p99'un ~760 ms'si kalır.
(e) *Adımlarda geri çekilme yok (sabit 50 ms)* — B2 öncesi; uzun yolda her
adım RTT/50 kopya yollar, gerçek bir tıkanıklıkta 20 Hz'de döver.

*Ölçüm (2026-10-02, release, `29059ca` — B86 + B85 + rapor, `main`
6a00d31'in — B88 düzeltmesi dahil — üstünde).* Yukarıdaki B2 tablosunun son
iki satırı; aynı komut (rUDP 1000 katılma fırtınası, iki kol dönüşümlü,
üçer koşu), **her koşudan önce yükün 1 dk ortalaması < 5** (2,9–4,9;
makinede başka iş yoktu). Her koşuda `connected = joined = left = 1000`,
`server_closes = 0`, `snap_total` 230 355–234 205, `errors = 0` ve bütün
`errors_*` anahtarları 0 (`errors_not_in_room` dahil — B88'in düzeltmesi;
B2'de 49–114). *Okuma:* (1) **B86:** varsayılan arabellekte connect p99
357–557 ms — B2 sonrasının 755–769 ms'sinden ~%30–50 kısa, ama B2
öncesinin (sabit 50 ms) 149–216 ms'sinin üstünde: adımlar yine 50 → 100
→ 200 ms geri çekiliyor, fırtınada üç-dört adım kaybeden istemci
~350–550 ms bekliyor. Bu kasıtlı: tavan geri çekilmeyi kaldırmadı,
sınırladı. 4 MiB'de 23–25/48–58 ms — B2'nin 19/51 · 21/59'u bandında, el
sıkışma yeniden denemesi 0. (2) **Bant devralmıyor:** istemci
`retrans_out` 1523–1890 (B2 sonrası 1347–1618, öncesi 2096–2228) —
fırtınada kaybolan kontrol kareleri artık tabandan yenileniyor. (3)
**B85:** sistem geneli `RcvbufErrors` 4335–4974'ün 3557–4196'sı sunucunun
soketi (%82–84), kalan ~780 loadgen çocuklarının istemci soketleri. (4)
**Rapor:** her oturum duyurdu (`udp_game_announces_received = 1000`);
sonda 10,6–11 bin, rapor 7,4–8 bin; `udp_game_probes_open_at_end` ≈ 3000
(oturum başına 3: loadgen LEAVE'den sonra okumayı bırakıyor — kesilme,
kayıp değil), `udp_game_probes_unanswered` varsayılanda 47–174 (sonda ya
da raporun fırtınada sunucu kuyruğunda kaybı), 4 MiB'de 0;
`udp_game_datagrams_reported_lost = 0` (457–475 bin oyun datagram'ında —
loopback'te kayıp yalnız sunucunun ALMA kuyruğunda, istemci → sunucu
yönünde); ortalama sonda turu 0,7–2,7 ms. Önceki (yük 13–53 altında,
`b745890`) koşular aynı yönü gösteriyordu (p99 369–613 ms, `errors`
10–68 — B88 öncesi); bu satırlar onların yerini aldı.

*Testler (önce kırmızı; mutasyonlu).* `rel::rto::tests::
{a_seed_keeps_the_estimate_and_drops_the_backoff,
a_handshake_step_backs_off_to_the_cap_not_the_ceiling}`;
`tests::handshake::cap::an_unanswered_step_is_re_sent_at_the_cap`
(duraklatılmış saat, sessiz eş: 1 sn'de tam yedi kopya — tavansız beş);
`tests::handshake::rtt::{a_handshake_whose_every_step_was_re_sent_takes_
no_sample (artık bant başlangıç zamanlayıcısında — eskiden geri çekilmiş
zamanlayıcıyı bekliyordu; sözleşme değişti), a_clean_challenge_seeds_the_
band_without_the_proofs_backoff}`; `a_proof_that_never_lands_…`'in üst
sınırı tavanlı takvime göre (27). Öldürülen mutasyonlar: tavanı kaldırmak
(2 test), bandı `seed` yerine `rto` ile kurmak (2), `seed`'in geri
çekilmeyi tutması (3).

**Oyun bandı geri bildirimi: sonda + alıcı raporu (rUDP sertleştirme 2 —
2026-10-02, bakımcı kararı: seçenek (a), EKLEMELİ rapor; B87 de burada
kapandı).** RAW ve FRAG asla ACK'lenmez ve sıra numarası taşımaz: sunucunun,
baytlarının neredeyse hepsini taşıyan bant için hiçbir kayıp ya da gecikme
sinyali yoktu — tıkanıklık denetiminin (B1, tur 3) okuyacağı şey. Ayrıca
seyrek kontrol bandında RTT tahmini bayatlıyordu (B87: fırtınada şişen
RTTVAR sonraki kontrol karesine dek sürüyordu). Kod: `udp::feedback`
(sunucunun saf durumu), `udp::writer::feedback`, `udp::demux::report`,
`udp::client::report`.

*Tel — iki YENİ datagram türü, var olan her bayt aynı* (§5'in evrim
kuralı; RAW/FRAG/REL/ACK/HELLO testlerle bayt-bayt sabit):

| bayt | `5` PROBE (sunucu → istemci) | `6` REPORT (istemci → sunucu) |
|---|---|---|
| 0 | tür = `5` | tür = `6` |
| 1–4 | sonda id'si, u32 LE (oturum başına 1'den artar, 0 hiç kullanılmaz) | cevapladığı sonda id'si, u32 LE (`0` = *duyuru*: henüz sonda yok) |
| 5–8 | yankı: sunucunun bu yoldaki EN YENİ RTT örneği, µs, u32 LE (`0` = önceki sondadan beri örnek yok) | oturum başından beri alınan oyun bandı datagram'ı (RAW ve FRAG, her biri bir), u32 LE, sarmalı |
| 9+ | yok — alıcı sondaki baytları yok sayar (ileride eklemeli alan) | aynı |

Her ikisi 9 B; demux 9 B'den kısa REPORT'u bozuk sayar.

*Akış.* (1) **İstemci katılmayı ister:** bağlanınca (kabulden hemen sonra)
bir duyuru (`REPORT{0, n}`) yollar; sonda gelmezse her 1 sn'de bir
(`ANNOUNCE_EVERY`), en çok 3 kez (`ANNOUNCE_MAX`). (2) **Sunucu kadansı
belirler:** duyuru yapan oturumun yazıcısı hemen bir sonda, sonra her
`PROBE_INTERVAL`'de (1 sn) bir sonda yollar; yazıcı zaten en geç her
50 ms'de uyanır (yeni await yok), sonda o geçişte senkron gider
(`try_send_to`, yeniden gönderim geçişi gibi). (3) **İstemci her sondayı
hemen cevaplar:** sondanın id'si ve o ana dek aldığı oyun datagram'ı
sayısı; yankı > 0 ise kendi REL bandının tahmincisine örnek olarak verir.
(4) **Demux** REPORT'u ACK gibi oturumun yazıcısına çıkış kanalından
`UDP_REPORT` (13) karesi olarak geçirir — aktör onu hiç görmez; rapor
boşta kalma penceresini tazelemez (ACK de tazelemez: canlılık istemcinin
kendi trafiği). (5) **Yazıcı** raporu uygular. Kadansı değiştirmek (tur
3: RTT başına, gönderim hızına göre…) yalnız sunucu değişikliğidir —
istemci saf bir yankıdır.

*Sıra numarası olmadan kayıp.* Yazıcı soketin kabul ettiği her oyun
datagram'ını sayar (`S`); her sonda gönderildiği anki `S`'yi tutar. Art
arda cevaplanan iki sonda `k-1, k` bir aralık sınırlar: gönderilen =
`S(k) − S(k-1)`, alınan = iki raporun farkı, kayıp = fark. Sondayı geçen
(sondadan sonra gönderilip önce varan) datagram'lar aralığın alınanını
gönderileninden birkaç fazla yapar: o fazlalık **sonraki aralığa taşınır**
— bu aralığın kaybı değil, sonrakinin teslimi değil (kırpılsaydı sonraki
aralık o kadar kayıp gösterirdi). İlk aralık oturum başından (`S = 0`,
alınan 0) ilk cevaplanan sondaya dek. Sonda yeniden gönderilmez; halkada
en çok `PROBE_RING` (4) bekleyen sonda; daha yeni bir sondanın cevabı
eskileri "cevapsız" sayar.

*Doğrulama — bir istemcinin iddiası durumu zehirleyemez* (her şey kendi
oturumunda, saf durum, `udp::feedback::tests`): duyurmamış oturumdan ya da
hiç gönderilmemiş id'li rapor → `invalid` (hiçbir şey uygulanmaz; "gönderildi"
= sonraki id'den geriye doğru son `probes_sent` id'den biri — sıradaki id ve
üstü geçersiz, sahte id `late`'e saklanamaz; id `u32::MAX`'tan 1'e sararken
0'ı atlar, kural sarmayı da kapsar); geriye
giden sayaç (sarmalı fark > 2³¹) → `invalid`; cevaplanmış ya da geride
kalmış sondanın raporu (yeniden sıralanmış/çiftlenmiş) → `late`, yok
sayılır; rapor geldiği ana dek sunucunun GÖNDERDİĞİNDEN fazlasını
iddia ederse → gönderilene **kırpılır** (`clamped`; ağın çiftlediği
datagram ya da yalan) ve uygulanır — kırpma sonraki aralığa kredi
bırakmaz. Bir istemci yalnız kendi oturumunun tahminini bozabilir; tur
3'te bu, kendi akışının hızı demektir.

*RTT ve B87 — örnek REL bandının tahmincisini de besler.* Sondanın turu
(`şimdi − sondanın gönderimi`; demux → yazıcı kanalı dahil, ACK örneği
gibi) hem oyun bandı tahminine hem oturumun `rel::Rto`'suna gider: tek
yol, tek tur, tek tahmin. Sonda asla yeniden gönderilmez ve rapor onu
adıyla anar — örnek Karn kuralını yapısı gereği sağlar; bandın kendi
kuralları (kendi karelerinde Karn, katlama, boşta kalma) DEĞİŞMEDİ,
yalnız geçerli bir örnek daha geldi (ve geçerli her örnek gibi geri
çekilmeyi bitirir — RFC 6298). İstemci tarafı: sonda sunucunun en yeni
örneğini yankılar, istemci onu kendi bandına örnek verir — iki yön de
dakikada bir kare taşıyan bantta saniyede bir tazelenir. 5 sn'den
(`REL_NO_ACK_FATAL`) uzun yankı canlı bir bandın turu değildir:
reddedilir ve sayılır (`probe_echoes_refused`). Fırtınada şişen RTTVAR
artık örnek başına ¾ ile söner (B87'nin ölçtüğü srtt 42 / rto 648 ms:
~5 örnekte rto < 200 ms) — sonraki kontrol karesini beklemeden.

*Uyumluluk, iki yön.* **Yeni istemci → eski sunucu:** eski demux
bilinmeyen türü `_ => bad_datagrams` koluyla düşürür ve
`udp_datagrams_malformed`'ta sayar ("bilinmeyen tür"); oturuma dokunmaz
(boşta kalma penceresine bile). Sonda gelmediği için istemci 3 duyurudan
sonra susar: eski sunucuda oturum başına en çok 3 sayım, başka etki yok
(95e6342 koduna karşı doğrulandı — aşağıda). **Eski istemci → yeni
sunucu:** duyuru yok → sonda yok → tel bugünküyle bayt-bayt aynı; oturumun
tahmini yok (`game_estimate() = None`), başka hiçbir şey değişmez.
İstemcide raporlar varsayılan AÇIK (`UdpClientConfig::game_reports`;
`UdpClient::connect_with` ile kapatılır — kapalı istemci eski istemcinin
kendisidir: duyurmaz, sondayı cevaplamaz). Eski istemci sondayı zaten
`_ => false` ile yok sayar (yeni sunucu ona sonda yollamaz). Yeni sunucu
da bilmediği türü (7+) aynı kuralla sayar, oturuma dokunmaz (test).

*Bedel (sınırlı).* Sonda ve rapor 9 B: IPv4'te başlıklarla 37 B, saniyede
bir — oturum başına her yönde **37 B/sn** (IPv6'da 57), artı oturum başına
en çok 3 duyuru. 30 Hz'lik bir snapshot akışında aralık ~30 datagram
kapsar (kayıp çözünürlüğü ~%3). Oturum başına durum: 4 girdilik halka ve
bir düzine sayaç. 100k oturumda tek sokete ±200k pps (oyun bandının 30 Hz'de
3M'sinin ~%7'si); tur 3 kadansı oturum başına seyreltebilir.

*Tahmin (tur 3'ün okuyacağı).* `feedback::GameEstimate`: en yeni ve en
küçük sonda turu, son aralığın uzunluğu / gönderileni / kaybı, düzleştirilmiş
kayıp kesri (aralık başına ¼ ağırlık; gönderimsiz aralık saymaz),
cevaplanan sonda sayısı; düzleştirilmiş RTT bandın `Rto::srtt`'si
(sondalar besliyor). Yazıcıda `game_estimate()`; davranış henüz
değişmedi — yalnız oturum sonu log'unda (`game_reports`, `game_loss`,
`game_min_rtt_us`). Tur 3 onu okur: aşağıda "Tıkanıklık tepkisi"
(tahmine aralığın baytları ve pencereli en küçük tur eklendi).

*Sayaçlar (taşıma kapsamı, OPS §3):* `udp_game_announces_received`,
`udp_game_probes_sent`, `udp_game_probes_send_failed`,
`udp_game_probes_unanswered` (oturum yaşarken raporu gelmeyen: sonda ya
da rapor kayboldu, daha yeni sonda önce cevaplandı),
`udp_game_probes_open_at_end` (oturum biterken hâlâ cevap bekleyen —
kesildi, kayıp değil; ayrı ki `unanswered` kayıp sinyali kalsın; yazıcılar
bitince `probes_sent = reports_received + probes_unanswered +
probes_open_at_end`), `udp_game_reports_received`,
`udp_game_reports_late`, `udp_game_reports_invalid`,
`udp_game_reports_clamped`, `udp_game_reports_not_forwarded` (demux:
yazıcı kanalı dolu/kapalı), `udp_game_datagrams_reported_sent` /
`udp_game_datagrams_reported_lost` (kayıp oranı = ikisinin oranı; gauge
değil: sayaçlar toplanır, oran sorgu anında — B32'nin "her kaybı say"
kuralı), `udp_game_rtt_samples` / `udp_game_rtt_sum_us` (ortalama =
ikincisi ÷ birincisi). İstemci (`UdpClientStats`):
`game_datagrams_received`, `probes_received`, `reports_sent`,
`announces_sent`, `reports_send_failed`, `probe_echoes_refused`.
`udp_datagrams_no_session` artık oturumsuz adresten gelen REPORT'u da
sayar (HELP güncellendi).

*Elenenler.* (a) *RAW/FRAG'a sıra numarası* — var olan baytları değiştirir
(evrim kuralı yasaklar; eski istemci her oyun karesini bozuk okurdu).
(b) *Sıralı yeni bir oyun türü (RAW2 `[u32 seq]…`)* — eklemeli, ama oyun
datagram'ı başına 4 B ve sunucuda istemci yeteneğine göre iki kodlama
yolu; sayım + sonda aynı kaybı oyun datagram'ı başına 0 baytla ölçer
(bedeli: kayıp aralık başına bilinir, hangi datagram olduğu değil —
tıkanıklık denetimi için yeter, oyun zaten kendini iyileştiriyor). (c)
*İstemcinin kendi saatiyle raporlaması (RTCP RR şekli)* — RTT için yine
sunucudan zaman damgalı bir şey ve bir "bekleme süresi" (DLSR) alanı
gerekirdi; kadans istemcide kalır, tur 3 onu sunucudan değiştiremezdi;
eski sunucuya sürekli bozuk datagram. (d) *Duyurusuz herkese sonda* —
eski istemciye fazladan datagram: "eski istemci için hiçbir şey değişmez"
bozulurdu. (e) *Rapor RTT'sini yalnız ayrı bir oyun bandı tahmininde
tutmak* — aynı yol için iki tahmin, B87 açık kalırdı. (f) *Raporu REL
bandında taşımak* — güvenilir bant bayat raporu yeniden gönderir, yeniden
gönderilen raporun turu örnek olamaz (Karn), aktör katmanına opcode girer.
(g) *Kaybı gauge (kesir) olarak yayımlamak* — oturumlar arası
toplanamaz; sayaç çifti oranı sorgu anında verir. (h) *Daha sık sonda
(her N datagram'da)* — 100k oturumda pps bedeli; kadans sunucunun, tur 3
gerektiği yerde sıklaştırır. (i) *Bayt sayımı* — tur 3 bayt temelli hız
isteyebilir; REPORT'un sonu eklemeli alana açık, şimdilik datagram.

*Testler (önce kırmızı; mutasyonlu).* `udp::feedback::tests` (saf durum,
sentetik saat: duyurusuz oturum hiç sondalanmaz; kadans; iki sonda → kayıp
ve RTT, yankı bir kez; düzleştirme; yeniden sıralama fazlası taşınır;
iddia kırpılır ve sonraki aralığa kredi bırakmaz; geriye giden sayaç
reddedilir; bilinmeyen id geçersiz, cevaplanmış geç; her sonda ya
cevaplanır ya cevapsız ya da oturum sonunda açık sayılır), `udp::writer::tests` (cevaplanan sonda
REL bandının ilk örneği, geri çekilmeyi bitirir — B87 sunucu tarafı),
`udp::client::tests::report` (sonda anında sayıyla cevaplanır, yankı
örnek — B87 istemci tarafı; 5 sn'lik yankı reddedilir; duyurular sınırlı,
ilk sondada biter; raporlar kapalıyken sonda yok sayılır),
`udp::demux::tests::report` (rapor yalnız yazıcıya, idle'a dokunmaz;
kısa/oturumsuz/dolu/kapalı sayılır; bilinmeyen tür bozuk, oturum
dokunulmaz), `udp::tests::feedback` (gerçek soket: yazıcı duyurudan önce
sondalamaz, 8 oyun datagram'ında 2 kayıp — kontrol karesi sayılmaz;
gerçek kapıda raporlayan istemci sondalanır ve ölçülür; raporlamayan
hiç sondalanmaz), `writer_lost`'un B73 testi rapor karesini de dışarıda
bırakır. *Eski sunucu kanıtı:* 95e6342 ağacına konan geçici bir demux
testi (commit edilmedi) bu turun duyurusunu (`[6, 0,0,0,0, 3,0,0,0]`)
`bad_datagrams`'a (`udp_datagrams_malformed`) sayıyor; oturum, boşta
kalma penceresi ve aktör dokunulmuyor, oturumun sonraki RAW'ı iletiliyor.
Öldürülen mutasyonlar 22'de 21: duyurusuz oturumu sondalamak (10 test —
eski baytları sabitleyen testler dahil), fazlayı taşımamak, kırpmamak,
geriye gideni kabul, geride kalanı/halka taşmasını/oturum sonunu
saymamak, geç/geçersiz karışması, yankıyı sıfırlamamak, REL'i
beslememek, kontrol datagram'ını oyun saymak, demux'ın raporu
düşürmesi, raporu oturum karesi saymak, FRAG'ı saymamak, istemcinin
sayaç tutmaması, duyuru tavanı yok, kapalıyken cevaplamak, sınırsız
yankı, ilk sondayı hemen göndermemek, kapalı yazıcıda oturumu tutmak.
Bakımcının sonradan bulduğu sağ kalan (`id < next_id` → `id <= next_id`:
sıradaki, hiç gönderilmemiş id `late` sayılıyordu) kuralı kesinleştirdi
(`Feedback::was_sent`) ve iki testle kapandı
(`ids_not_yet_sent_are_invalid`, `the_id_wrap_keeps_late_and_invalid_apart`;
sarmada 0'ı atlamayan hesap da öldü). Sağ çıkan: bağlanırken duyuruyu kaldırmak — ilk yeniden gönderim
geçişi (en geç bir okuma) duyuruyu zaten yollar; eşdeğer davranış.

**Tıkanıklık tepkisi (rUDP sertleştirme 3 — 2026-10-02, BACKLOG B1;
bakımcı kararları).** Tur 2 sinyalleri getirdi (`GameEstimate`, sondalar,
`udp_game_*`); bu tur onlara göre davranır. Kod: `udp::congestion` (saf
`Control` ve `PaceQueue`, genel `PathState`/`PathPhase`/`UdpCongestion`),
`udp::writer::pace` (yazıcının kablolaması). Tel DEĞİŞMEDİ: yeni tür yok,
yeni alan yok (REPORT'a bayt sayımı da gerekmedi — aşağıda).

*Kural (bakımcı).* **Taşıma ölçer ve hızlar; içeriği inceltmek oyunun
(kitin) opt-in kararı.** Raporlayan bir oturumun tahmini yol hızı odanın
gönderdiğinin altındaysa yazıcı sınırsız kuyruk tutmaz (o gecikmedir):
oyun bandını tahmini hıza göre **hızlar** (pacing) ve gönderemediği **EN
ESKİ** oyun bandı karelerini düşürür — her biri adıyla anlamı aynı bir
sayaçta (`udp_game_frames_dropped_paced`). Kontrol bandı (REL) asla
hızlanmaz, asla düşmez, asla oyun kuyruğunun arkasında beklemez: baytları
aynı kovadan düşülür, oyun bandı ona yer açar. Oyunun ne göndermesi
gerektiği taşımanın kararı değildir; taşıma yolun ne taşıdığını söyler
(`PathState`, aşağıda "Oyuna sinyal").

*Açma/kapama (opt-in).* Sunucu anahtarı `udp_congestion = "off" | "pace"`
(OPS §2); `UdpTransportConfig::congestion`. Varsayılan `"off"`: yazıcı
bugünkünün aynısı (denetleyiciye hiç dokunulmaz). `"pace"`'te bile
**raporlamayan istemci** (eski istemci, `game_reports: false`) tahmin
üretmez, hep `Open` kalır: bugünküyle bayt-bayt, sırasıyla aynı
(`udp::tests::pace::a_client_that_does_not_report_gets_the_same_bytes`
— aynı partiler `off` ve `pace` yazıcısından, 200 datagram RAW/FRAG/REL
karışık, karşılaştırılır). Yolu yetişen raporlayan oturum da `Open`'dır:
oyun bandı yine anında gider (ölçüm: darboğazsız koşu iki modda aynı).

*Durum makinesi (her uygulanan raporda — `Control::on_estimate`; tur 4'ün
hâli, B104 — aşağıda "Gecikme sinyali, artış ve taban — tur 4").* Eşik =
5 × yolun titreşimi, 30–300 ms arasında (`signal::RttTrack::threshold`);
"ayakta kuyruk" = en yeni turların küçüğü pencereli tabanın eşik kadar
üstünde.

| evre | sinyal yok | sinyal |
|---|---|---|
| `Open` (hızlanmaz, sonda 1 sn) | `Open` | `Suspect` — ipucu: en yeni tur tabanın 30 ms üstünde ya da kayıp sinyali |
| `Suspect` (hızlanmaz, sonda 250 ms) | 4 raporda ayakta kuyruk yoksa `Open` | `Paced`, hız = teslim × β — üst üste iki kayıp sinyali, ya da en yeni 2 turda ayakta kuyruk ve titreşim ≥ 4 hızlı örnekle bilinir (kuyruk ≥ 300 ms ise beklemeden) |
| `Paced` (hızlanır, sonda 250 ms) | hız += ¼ × bölümün en yüksek talebi/sn; hız ≥ 1,25 × o talep → `Open` | kayıp ya da en yeni turda kuyruk: hız = min(hız, teslim) × β (en yeni tur 5 ms'den çok düştüyse — boşalıyor — ve kayıp yoksa: hız tutulur, artmaz da) |

Bir halka dolusu (4) sonda cevapsız kalırsa (`Paced`'te): hız yarıya
(örnek yok — klasik zaman aşımı tepkisi). Taban: saniyede 1 datagram
bütçesi (1472 B/sn; tur 3'te 4 — tur 4'te düşürüldü).

*Kararlar ve gerekçeleri (ve elenenler).*

1. **Kayıp mı gecikme mi — ikisi de.** Yalnız kayıp: derin tamponlu bir
   darboğaz (bufferbloat) taşana dek kayıp vermez — tepki geldiğinde
   gecikme yarım saniyeyi aşmıştır (ölçüm: derin tampon, `off`: oyun
   bandı p50 527 ms). Yalnız gecikme: politika uygulayan (policer) ya da
   sığ tamponlu yol gecikme büyütmeden düşürür. Kayıp sinyali: aralıkta en
   az 2 datagram ve en az %10 kayıp (`LOSS_MIN`, `LOSS_DIV`) — tek kayıp
   ve %10 altı gürültü (Wi-Fi'nin rastgele kaybı tepki tetiklemesin).
   Gecikme sinyali: en yeni sonda turu pencereli en küçük turun 30 ms
   üstünde (`QUEUE_DELAY_LIMIT`) — oyunun hissettiği ayakta kuyruk.
   *(Tur 4: tek örnek titreşimli yolda yanlış alarmdı — B104 ölçümü; artık
   en yeni turların küçüğü, yolun titreşimine göre eşikle — aşağıda
   "Gecikme sinyali, artış ve taban — tur 4".)*
2. **Patlama mı süreklilik mi.** Tur 1'in bulgusu (patlamada geri
   çekilmek gecikmeyi büyütür) burada: tek sinyal yalnız şüphedir
   (`Suspect`: sonda 250 ms'ye sıklaşır, hiçbir şey hızlanmaz/düşmez);
   üst üste ikinci sinyal hızlandırır, temiz aralık şüpheyi siler. Bir
   kötü aralığın bedeli birkaç fazla sondadır (9 B). *(Tur 4: şüphe 4
   rapor sürer; hızlı örnekler titreşim tahmininin girdisidir.)*
3. **Hız nasıl tahmin edilir — teslim edilen hız, AIMD düzeltmesiyle.**
   Oyun bandı en-yenisi-kazanır ve uygulama-sınırlıdır: gönderilen, yolun
   taşıyabileceğini söylemez; *teslim edilen* söyler. Aralığın teslimi =
   gönderilen baytlar × teslim oranı (rapor datagram sayar); süresi =
   aralığın istemcinin gördüğü uzunluğu: gönderim aralığı + turun o
   aralıktaki büyümesi (dolan bir kuyruk aynı datagramları daha uzun
   alım süresine yayar — büyüyen kuyruk kapasite sanılmaz; test
   `a_standing_queue_is_a_signal_and_a_growing_one_not_capacity`, tur 4'teki adıyla). Giriş ve her sinyal:
   hız = teslim × β (0,85), mevcut hızın üstüne asla. Temiz aralık:
   saniyede 16 datagram bütçesi/sn **toplamsal** artış (1472 B'de ≈ 23,5
   KB/sn²) — paylaşılan darboğazda oturumların eşit paya yakınsaması
   bundandır (Chiu–Jain: toplamsal artış, çarpımsal azalış; test
   `two_sessions_on_one_bottleneck_converge_to_equal_shares`). *(Tur 4:
   artış bölümün en yüksek talebinin ¼'ü/sn — röle testinin 91 KB/sn'lik
   talebinde tur 3'ün adımı, 64 oturumda onun 1/7'si; açılma o talebin
   1,25 katında; boşalma tutması artışı da durdurur; alım süresini
   uzatan, tek örneğin değil en yeni iki turun küçüğünün büyümesi.)* **Boşalan
   kuyruk tutar:** `Paced`'te yalnız gecikme sinyali varken tur bir
   öncekinden kısaysa önceki kesinti çalışıyordur — hız tutulur (kayıp
   yine keser). Ölçüldü: tutmasız 4 oturumlu paylaşımlı koşuda 200–211
   kesinti ve 6,81–6,92 MB teslim; tutmayla 149–164 kesinti ve 7,03–7,06
   MB. **Elenenler:** (a) *gönderilen hızdan saf AIMD (TCP Reno şekli)* —
   uygulama-sınırlı bantta pencere/hız kullanılmayan değere büyür
   (RFC 7661'in sorunu) ve kapasiteye tek adımda değil testere dişiyle
   iner; (b) *BBR'nin tamamı* — datagram başına teslim zaman damgası
   ister (RAW'a sıra numarası: tur 2'de elendi), kazanç döngüsü ve
   ProbeRTT 250 ms'lik tek örnekle anlamsız; teslim hızı fikri alındı;
   (c) *WebRTC GCC (gecikme eğimi)* — paket başına varış zamanı geri
   bildirimi (transport-wide CC) ister: tel değişikliği; (d) *yalnız
   gecikme (LEDBAT/Vegas)* — policer'da kör, gürültülü yolda ürkek.
4. **Kuyruk ve düşürme: en eskisi, bütçe 50 ms.** Hızlanan oturumun
   sunucu kuyruğu en çok hız × 50 ms bayt tutar (`QUEUE_BUDGET`) ve HER
   ZAMAN en yeni mesajı: en-yenisi-kazanır — yeni snapshot eskisini
   geçersiz kılar, eskisini geç göndermek bant harcar. Bütçe aşılınca
   önce en eski düşer, sayılır. **Elenenler:** sınırsız kuyruk
   (gecikme — bakımcı yasakladı), en yeniyi düşürmek (kuyruk sonu
   düşürme: bayat kare gönderir), sayıya göre sınır (boyuttan habersiz:
   tek büyük tam snapshot ile on küçük delta aynı sayılırdı).
5. **FRAG atomikliği: hep ya hiç.** Parçalı mesaj kuyruğa bütün
   datagram kümesi olarak girer; bütün düşer; ilk parçası tele çıkmış
   mesaj asla düşmez (kalan parçalar onu işe yarar kılan şeydir — yerine
   bir sonraki en eski düşer); oturum biterken yarım kalan
   `udp_game_frames_unsent_paced`'te BÜTÜN bir kare sayılır (test
   `a_fragmented_message_is_all_or_nothing`). Ölçüm: derin tamponda
   `off`'ta darboğazın kuyruk sonu düşürmesi 268 mesajı yarım bıraktı
   (taşınan parçalar boşa bant), `pace`'te 26.
6. **Tek soketi paylaşan oturumlar arasında adalet: paylaşılan
   zamanlayıcı yok.** Her oturumun yazıcısı kendi denetleyicisini ve
   kuyruğunu taşır; adalet paylaşılan darboğazda toplamsal-artış /
   çarpımsal-azalışın yakınsamasından gelir. Ölçüm (4 oturum, 1 sn arayla
   katılan, toplam talebin yarısı kapasite): 40 sn'lik koşunun son 15
   sn'sinde Jain endeksi 0,996–0,998, kapasitenin %98'i teslim; ilk
   katılımlardan sonraki ~20 sn yakınsama (18 sn'lik koşunun son 8
   sn'sinde 0,74–0,97 — geç gelen oturumun penceresi ayakta kuyruğu
   "taban" sanar, pencere dönünce düzelir). **Elenen:** soket genelinde
   bir DRR/adil kuyruk aktörü — her datagram için bir kanal sekmesi daha
   ve yazıcılar arası paylaşılan durum (§2); sunucunun kendi çıkışı
   darboğaz olursa (100k'da) ayrıca ölçülmeli (yeni satır).
7. **Hızlama tanesi ve tek bekleme.** Jeton kovası: derinlik hız × 10 ms,
   en az bir datagram (`PACE_BURST`); datagram datagram bırakır. Hızlama
   son tarihi yazıcının var olan `min`'ine katılır (yeniden gönderim
   zamanlayıcısı, en geç 50 ms tick): `timeout(min(rto, tick,
   pacing), out_rx.recv())` — yine tek beklenen kaynak, yeni görev yok.
   Kontrol baytları kovadan düşülür (borç olabilir); sondalar düşülmez
   (9 B).
8. **Sonda kadansı ve sessiz istemci (B91).** `Open`: 1 sn (değişmedi —
   yolu yetişen oturumun baytları aynı). `Suspect`/`Paced`: 250 ms
   (`FAST_PROBE_INTERVAL`; 30 Hz'de ~8 datagram/aralık — kayıp çözünürlüğü
   kaba ama tepki ~1,25 sn). **B91:** halka dolusu sonda cevapsız
   kaldıktan sonra her tahliyede aralık ikiye katlanır, en çok 8×
   (`SILENT_BACKOFF_MAX`); ilk cevap kadansı geri getirir. Okumayı bırakan
   istemci artık saniyede değil 8 sn'de bir sonda alır. İki modda da
   geçerli: sonda bedeli düzeltmesidir, hızlama değil — hiç raporlamayan
   istemci zaten hiç sondalanmaz (test
   `a_silent_client_is_probed_ever_less_often`).
9. **Pencereli en küçük tur (B93).** `GameEstimate::window_min_rtt`: iki
   5 sn'lik kova, pencere 5–10 sn (`RTT_WINDOW`); bir pencere boyu sessizlik
   ikisini de siler. Ömür boyu `min_rtt` log için kaldı. Ömür boyu en
   küçük, uzayan bir rotayı sonsuza dek "ayakta kuyruk" sanardı.
10. **REPORT'a bayt sayımı: gerekmedi.** Sunucu her aralıkta gönderdiği
    baytları bilir (`GameEstimate::interval_sent_bytes`); teslim =
    gönderilen bayt × teslim oranı. Hata payı: kaybolan datagram'ların
    boyu ortalamadan farklıysa (büyük FRAG / küçük RAW karışımı) —
    toplamsal düzeltme onu birkaç aralıkta emer. Tel değişmedi.

*Oyuna sinyal (tur 3: gsb-net sınırında).* `gsb_net::udp::PathState`
(`Copy`, küçük): `phase` (`Open`/`Suspect`/`Paced`), `rate` (yalnız
`Paced`'te, B/sn), `demand` (odanın son rapor aralığında oyun bandına
verdiği, B/sn), `loss_permille`, `queue_delay`; `budget(period)` — bir
tick'te/snapshot aralığında yolun taşıyacağı bayt. Yazıcı her kararda
günceller (`UdpWriter::path_state`), evre/hız değişince `debug` log,
oturum sonu `info` satırında. Çekirdeğe taşınması B103 (aşağıda).

**Yol sinyali çekirdekte ve kitte (B103 — 2026-10-02; bakımcı
kararları a–d).** Taşıma ölçer, çekirdek taşır, oyun (kit) karar verir.

```text
taşıma ──ConnIn::Path──▶ bağlantı aktörü ──(üyenin action kanalı, MEMBER_PATH)──▶ oda / shard READ
 (yalnız haber, try_send)   (en-yenisi-kazanır)                                      │
                                                                     PathTable ──▶ TickCtx::{budget, path}
                                                       fan-out: GameLogic::ship_snapshot ◀── kit SnapshotBudget
```

*(a) Taşımadan bağımsız `gsb_core::path::PathState`.* `phase`
(`PathPhase::{Open, Suspect, Paced}`), `rate: Option<u32>` (B/sn, yalnız
taşıma yolu sınırlarken — `Paced`), `demand`, `loss_permille`, `rtt`,
`queue_delay` — evre dışındaki her alan `Option`: her taşıma ölçtüğünü
doldurur. rUDP hepsini (aşağıda, "Faz 2"), QUIC quinn'in kendi
istatistiklerinden (aşağıda), TCP/TLS/WS hiçbirini — yol durumu
göndermeyen taşımanın üyesinin yolu bilinmez (`None`: oyun bugünkü gibi
gönderir). Akış kapılarına çekirdeğin `TCP_INFO`'su ileride bir kaynak
olabilir; bu turun işi değil. Faz 2'den beri rUDP bu tipin kendisini
kullanır (`gsb_net::udp::{PathPhase, PathState}` onun yeniden
ihracı).

*(b) Odanın birincil sorusu bayt bütçesi.* `TickCtx::budget(member) ->
Option<usize>`: üyenin yolunun bu tick'te taşıyacağı bayt (`rate` ×
odanın tick periyodu); `None` — sınırlı değil ya da bilinmiyor (üye
değil, park, bot, ölçmeyen taşıma, yetişen yol). Tam durum da okunur:
`TickCtx::path(member)`. `TickCtx`'e yeni alan `paths: PathView` (elle
kurulan bağlam boş görünümü taşır — `Default`); aktörün `PathTable`'ı
idle saati gibi tick boyunca ödünç verilir. Boş tablo tick'e bir dal
maliyetindedir.

*(c) Yol: `ConnIn::Path` ve üyenin kendi action kanalı.* Taşıma
`ConnIn::Path(PathState)`'i yalnız HABER olduğunda gönderir (evre
değişti, hız belirdi/kalktı ya da son TESLİM EDİLENE göre ≥ %10 oynadı —
`PathState::moved_from`), `try_send` ile, asla beklemeden
(`PathSignal`: dolu posta kutusu en yeni durumu borçlu tutar, daha yenisi
onun yerini alır — en-yenisi-kazanır; durum olay değildir, yerini yenisi
alan durum hiç haber değildi). Bağlantı aktörü durumu odaya **üyenin
kendi action kanalında**, iç bir işaretçi olarak taşır
(`op::base::MEMBER_PATH = 15`, telde ASLA yok; istemciden gelen 15
bilinmeyen taban opcode'u gibi sert ihlaldir ve odaya varmaz — test
`a_client_frame_with_the_marker_opcode_never_reaches_the_room`). Aynı
kural: kanal doluysa (üyenin kendi girdisi önde) durum borçlu kalır ve
aktörün okuduğu her sonraki mesajda yeniden denenir (bekleme sınırı:
aktörün bir sonraki mesajı — girdi, heartbeat ya da yeni haber; durum
asla girdinin arkasında kuyruklanmaz); katılım (join) sinyali sıfırlar
— yeni oda hiçbir şey bilmez, en yeni durum ona borçludur (yeniden
katılım dahil). Oda/shard READ'i işaretçiyi çektiği anda ayırır: üyenin
`PathTable` satırına yazar; idle damgasına (girdi değildir), oyunun
`ingest`'ine ve okunmamış-girdi sayaçlarına (`actions_dropped_unread`)
asla girmez. Tablo üye oturumu bitince ya da park olunca boşalır
(ayrılış, despawn, kopuş/park, girdi-boşta tavanı; resume yalnız park
satırını yeniden bağlar, yeni oturum bilinmeyenle başlar); shard
geçişinde durum üyeyle taşınır (`PlayerMigration::path` — aktör yalnız
haber gönderdiğinden düşen durum bir sonraki değişime dek bilinmezdi).
**Neden bu yol (elenenler):** (i) *`RoomControl::MemberPath`* —
bağlantı aktörünün odanın kontrol kutusu yok; registry üzerinden yol
tekil aktöre sıcak yol yükü bindirir ve shard'lı odada üyenin hangi
shard'da olduğunu bilmez (her shard'a yayın); ortak kontrol kutusunu
yol haberleriyle doldurmak katılımları reddettirebilirdi. (ii) *Ayrı üye
başı yol kanalı* — her üye için her tick bir `try_recv` daha (varsayılan
yolda bedel) ve join yanıtının/`PlayerMigration`'ın biçimini ~100 yerde
değiştirirdi. (iii) *`Action`'a alan* — oyunlar `Action`'ı kendileri de
kurar (bot girdisi): kırıcı değişiklik. (iv) *Paylaşılan atomik/izleme
kanalı* — §2 (durum mesajla taşınır). Üyenin kendi kanalı: yalıtılmış
(dolu kanal yalnız bu üyenin durumunu geciktirir), göçte zaten taşınıyor,
oda onu zaten çekiyor — yeni yoklama yok. **Sayaç yok (bilinçli):**
burada hiçbir şey kaybolmaz — üyelik sürdükçe oda en yeni durumu alır;
biten üyeliğin söyleyeceği kimse yok.

*Fan-out kapısı (çekirdek, opt-in): `GameLogic::ship_snapshot`.*
Fan-out, bütçesi bilinen (`TickCtx::budget` `Some`) ve grubunun bu tick
karesi olan her üye için mantığa sorar: `ship_snapshot(world, ctx,
player, group, bytes, budget) -> bool`. `false` grup karesini o üyenin
batch'inden çıkarır (private karesi yine gider) ve sayılır:
`snapshots_withheld` (oda kapsamı). Bütçesi bilinmeyen/yetişen üyeye hiç
sorulmaz; varsayılan kanca gönderir — açmayan oyunun baytı değişmez.
Oda ve shard aynı yardımcıyı kullanır (`room::counters::fanout::ships_group`).
Mantık yalnız istemcinin onsuz yapabildiğini tutmalı: bağımsız tam
snapshot (sonraki iyileştirir), asla delta.

*Kit yapı taşı: `gsb_kit::budget::SnapshotBudget`* (KIT-ARCHITECTURE §10
"B103"): tam-snapshot odaları (açık, PVS, düz sharded) için üye başına
kare hızı inceltmesi — sığan her tick, sığmayan kredisi yetince; en az
16 karede bir (A10'un `Ticks16`'sı) bütçe üstü gider, sayılır
(`snapshot_budget_forced`, mantık-sayaç dikişi).

*QUIC: quinn'in istatistikleri* (`quic::path`): yazıcı bayt taşırken en
çok 250 ms'de bir `Connection::stats()` — `rtt` (yumuşatılmış),
`queue_delay` (5–10 sn pencereli tabana göre), aralığın kayıp/gönderilen
paketinden `loss_permille`, aralığın UDP baytından `demand`; evre rUDP'nin
makinesi, sinyal quinn'in `congestion_events`'i (biri şüphe, ikincisi
`Paced`); `Paced`'te `rate` = tıkanıklık penceresi / tur, pencere talebin
1,25 katını taşıyınca `Open`. Hız yalnız `Paced`'te söylenir: uygulama
sınırlı pencere büyümez, açık yolda bütçe vermek oyunu yolun
taşıyabileceğinin altında tutardı. Yeni görev yok (besleme gönderme
yarısında), posta kutusu beklenmez.

*(d) Varsayılan.* `udp_congestion = "off"` kaldı; B104 (titreşim ölçümü
ve bu tur) çevirir.

*Faz 2: rUDP yazıcısı yayımlar (B3'ten sonra, 2026-10-02).*

- **Tip: dönüşüm değil, çekirdeğin kendisi.** `udp::congestion`
  `gsb_core::path::{PathPhase, PathState}`'i yeniden ihraç eder; ayrı
  rUDP tipi silindi. Gerekçe: tek tip — dönüşümün alan alan kayması
  yok, `budget` tek yerde; eski tipin rUDP dışında kullanıcısı yoktu.
  *Elenen:* `From<udp::PathState>` — iki tip, iki `budget`, aynı
  alanların iki adı. `Control::state()`: evre ve (yalnız `Paced`'te)
  hız; bu yolda bir rapor uygulandıktan sonra ölçümler de — odanın
  talebi, düzleştirilmiş kayıp, en yeni tur (`rtt`), pencereli tabana
  göre kuyruk. Rapordan önce ve yeni yolda yalnız evre (varsayılan
  durum).
- **Yayım, yalnız `"pace"`'te.** Yazıcının her kararı — uygulanan rapor
  (`pace_report`), cevapsız halka (`pace_silence`) — `pace_follow`'dan
  `pace_tell`'e gider: durum `PathSignal`'e sunulur, haber ise
  `ConnIn::Path` olarak bağlantı aktörüne `try_send` edilir (asla
  beklenmez). Dolu gelen kutusu en yeni durumu borçlu tutar; bir sonraki
  kararda (sonda kadansı: açıkken en çok 1 sn, şüphe/hızlamada 250 ms)
  yeniden denenir. Kapalı kutuda söylenecek kimse yok. Not: yazıcı
  doğumda ölüm kararı için kutuda bir yuva ayırır (B66); haber kalan
  kapasiteyi kullanır.
- **Yeni IP (B3, RFC 9000 §9.4).** Denetleyici sıfırlanınca
  (`Control::new_path`) yazıcı sinyali de sıfırlar (`pace_new_path`):
  odanın bildiği eski yolundu, taze ve ölçümsüz `Open` durumu — eskisi
  de açık olsa — söylenir. Yalnız port değişimi aynı yoldur: hiçbir şey
  söylenmez.
- **`"off"` mesajı mesajına bugünkü gibi.** Yanıt kapalı yazıcı aktöre
  hiçbir şey göndermez — raporlar, cevapsız halka ve yol değişimi de
  dahil (birim testi `with_the_response_off_the_actor_hears_nothing`,
  gerçek soket `with_the_response_off_the_actor_is_told_nothing`).
  Raporlamayan istemci `"pace"`'te de hiç ölçülmez ve hiç söylenmez.
- **Sayaç yok:** yayım kayıpsızdır (en-yenisi-kazanır; durum olay
  değildir).
- **Testler (önce kırmızı; her kural mutasyonla).** `udp::congestion::
  tests::the_state_carries_the_measurements_once_reported`;
  `udp::writer::tests::signal` (4 — her haber bir kez, ölçüm tek başına
  haber değil, hız yarılanması haber; dolu kutuda en yenisi borçlu ve
  sonraki kararda gider; yeni IP taze `Open`, yeni port hiçbir şey;
  `"off"` hiçbir şey); `udp::tests::pace::signal` (gerçek soket, 2:
  `Open` → `Suspect` → `Paced`, hız tabanda 4 800 B/sn, 30 Hz'de 159 B — tur 4'ten beri taban 1 bütçe/sn ve hız teslimin β katı, ≈ 1,7–2,5 KB/sn;
  `"off"` boş); **uçtan uca** `gsb-server/tests/path_budget.rs` (2:
  `udp_congestion = "pace"` rUDP kapısı, kullanıcı alanı policer'ının
  — 8 KB/sn, 3 KB — arkasında raporlayan istemci, her tick hareket eden
  noktaların ~1,4 KB'lık tam snapshot'ı ile `SnapshotBudget`'lı kit
  `OpenRoom`'u: oyun `TickCtx::budget = Some` okur — üç koşuda 196–406
  B/tick, 47–48 tick —, oda `snapshots_withheld > 0` raporlar — 31;
  `"off"`'ta bütçe hiç yok, tutulan 0). Öldürülen mutasyonlar:
  `pace_follow` söylemiyor (4 test, uçtan uca dahil), kapalıyken
  söylüyor, dolu kutuda unutuyor, yeni yolda sıfırlamıyor, `apply_path`
  sessiz, durumda `rtt`/ölçümler/talep yanlış; uçtan uca ayrıca kit
  kapısının kaldırılmasını yakalar.

*Varsayılan: şimdilik `"off"`.* Ölçümlerde `pace` hiçbir senaryoda
kaybetmedi: darboğazda 2× mesaj, ~5× düşük gecikme, kontrol bandı ~10×
hızlı; darboğazsız koşuda aynı bayt ve zamanlama. Yine de bu tur
varsayılan çevrilmedi: (1) ölçümler yerel (loopback + kullanıcı alanı
darboğazı); gerçek bir ağda (netem/WAN) titreşimli yolda (Wi-Fi,
hücresel: 30 ms üstü titreşim) sahte gecikme sinyali ölçülmedi — o yolda
tepki gereksiz düşürmeye dönebilir; (2) oyunun sinyali yokken düşürme
oyuncunun göremediği bir şeydir (B103 sinyali çekirdeğe, kite ve rUDP
yazıcısına taşıdı). **Öneri:** titreşim
ölçümü temiz çıkarsa ve sinyal çekirdeğe taşındıktan sonra varsayılan
`"pace"` olsun (yeni satır). *(2026-10-03: titreşim ölçümü temiz çıkmadı
— tur 4 sinyali ve tabanı düzeltti; varsayılan yeniden ölçümü bekliyor:
aşağıda "Gecikme sinyali, artış ve taban — tur 4".)*

*Sayaçlar (taşıma kapsamı, OPS §3):* `udp_game_frames_queued_paced`
(hızlama kuyruğuna giren kare — her biri sonra gönderilir, düşer ya da
gönderilmeden kalır), `udp_game_frames_dropped_paced` (bütçe dolunca
düşen en eski), `udp_game_frames_unsent_paced` (oturum biterken kuyrukta
kalan — yazıcı durdu, REL bandı öldü ya da oturum bitti), defter:
`queued = gönderilen + dropped + unsent` (test
`every_queued_frame_is_sent_dropped_or_unsent` ve gerçek sokette
`a_paced_session_keeps_the_newest_and_never_queues_control`);
`udp_game_paced_episodes` (Open → Paced girişleri),
`udp_game_paced_rate_cuts` (hız düşüşleri: giriş, sinyal, cevapsız
halka). Loadgen telinin transport bölümü 5 sayaç uzadı: **GSNM**.

*Ölçüm (kullanıcı alanı darboğaz, `udp::tests::pace::relay`;
`udp::tests::pace::measure`, `--ignored`).* Gerçek kapı, raporlayan
`UdpClient`'lar, oturum başına bir oda: her 33 ms'de 3000 B snapshot
(3 FRAG datagramı, ≈ 91 KB/sn talep) ve her 200 ms'de bir kontrol karesi,
gönderim anıyla damgalı. Röle: istemci başına bir soket; aşağı yönde
TÜM oturumlar tek FIFO bağlantıyı paylaşır (hız, kuyruk sonu düşüren
tampon, 20 ms yayılım). Yaş = gönderim → istemcinin `recv_frame`'i.

Senaryolar: **derin** — 60 KB/sn, 30 KB tampon (500 ms), 1 oturum;
**sığ** — 60 KB/sn, 6 KB tampon (100 ms), 1 oturum; **paylaşımlı** — 4
oturum 1 sn arayla katılır, 180 KB/sn (toplam talebin yarısı), 90 KB
tampon; **darboğazsız** — 10 MB/sn. Ölçüm penceresi: derin/sığ 14 sn'nin
son 8'i, paylaşımlı 40 sn'nin son 15'i, darboğazsız 10 sn'nin son 6'sı.
2026-10-02, tek makine (`udp::tests::pace::measure`).

| senaryo | mod | mesaj/sn | KB/sn | oyun yaşı p50 / p95 / max ms | kontrol yaşı p50 / p95 / max ms | darboğaz düşürdü | yarım FRAG | `dropped_paced` / `queued_paced` | kesinti | Jain |
|---|---|---|---|---|---|---|---|---|---|---|
| derin | off | 9,0 | 27,0 | 527 / 533 / 537 | 518 / 531 / 537 | 281 | 268 | — | — | — |
| derin | pace | 18,5 | 55,5 | 96 / 116 / 131 | 41 / 60 / 66 | 25 | 25 | 148 / 362 | 11 | — |
| sığ | off | 9,0 | 27,0 | 119 / 123 / 129 | 111 / 122 / 122 | 298 | 293 | — | — | — |
| sığ | pace | 18,4 | 55,1 | 94 / 126 / 159 | 42 / 64 / 93 | 27 | 27 | 151 / 382 | 11 | — |
| paylaşımlı (oturum başına) | off | 0,1–23,5 | 0,4–70,4 | 525–529 (p50) | 527–16 133 (p50), en kötü max 20 123 | 334–2512 | 286–1050 | — | — | 0,274 |
| paylaşımlı (oturum başına) | pace | 13,2–15,9 | 39,6–47,6 | 102–106 / 132–144 / ≤168 | 45–47 / 69–78 / ≤93 | 1–5 | 1–5 | 2331 / 4101 | 154 | 0,993 |
| darboğazsız | off | 30,5 | 91,5 | 22 / 25 / 36 | 22 / 24 / 25 | 0 | 0 | 0 / 0 | 0 | — |
| darboğazsız | pace | 30,5 | 91,5 | 23 / 29 / 53 | 23 / 37 / 38 | 0 | 0 | 0 / 0 | 0 | — |

Okuma: 60 KB/sn'lik yol 3017 B'lik mesajdan en çok ~19,9/sn taşır —
`pace` 18,5'ini teslim eder (`off` 9: darboğazın kuyruk sonu düşürmesi
mesajların çoğunu yarım bırakır, yarım mesajın parçaları boşa bant);
oyun bandı yaşı derin tamponda 527 → 96 ms, kontrol bandı 518 → 41 ms
(kontrol karesi artık 500 ms'lik kuyruğun arkasında beklemiyor).
Paylaşımlı `off`'ta son katılan bağlantıyı yer (Jain 0,27), ötekilerin
kontrol bandı 16–20 sn geç kalır (242 yeniden gönderim) — REL ölüm
sınırına yakın; `pace`'te dört oturum eşit pay (Jain 0,993), kapasitenin
%97'si teslim. **Darboğazsız:** iki mod aynı yoldan geçer (oturum `Open`,
hiçbir şey kuyruğa girmez — `udp_game_frames_queued_paced = 0`): teslim
aynı (915 414 / 915 437 B — fark tek bir REL yeniden gönderimi), yaş
farkı makine gürültüsü (önceki koşu: off 22/23/25, pace 21/24/27, ikisi
de 915 414 B). Tekrarlanabilirlik: derin/sığ iki koşuda ±1 mesaj/sn;
paylaşımlı 40 sn'lik dört koşuda Jain 0,988–0,998 (ikisi boşalma tutması
olmadan: 0,988/0,993 — karar 3).

*Testler (önce kırmızı — her kural mutasyonla).* Kurallar önce saf durumda sentetik saatle
(`udp::congestion::tests` 8 test, `…::tests::queue` 5,
`udp::feedback::tests::cadence` 5), sonra yazıcıya kablolanmış haliyle
(`udp::writer::tests`: denetleyiciyi izler, kapalıyken asla, kesinti
kuyruğu hemen kırpar ve uyanmayı hızlayıcı belirler) ve gerçek sokette
(`udp::tests::pace`: raporlamayan istemci bayt-bayt aynı; hızlanan
oturum en yeniyi tutar, kontrol kuyruğa girmez, defter kapanır).
**Mutasyonlar 39'da 38 öldü** (her biri: dosya karalama dizinine
yedeklendi, bozuldu, hedefli testler koştu, yedekten geri yüklendi):
LOSS_MIN'i/oranı kaldırmak, gecikme sinyalini kaldırmak, patlamada hemen
hızlanmak, şüphenin hiç silinmemesi, alım süresi düzeltmesini kaldırmak,
hızın üstüne kesmek, artış yok, hiç açılmamak, taban yok, sessizliği yok
saymak, boşalma tutmasını kaldırmak, kayıpta da tutmak, β = 1; hiç
düşürmemek, teldeki mesajı düşürmek, en yeniyi düşürmek, kontrolü
kovadan düşmemek, derinliği datagram'ın altına indirmek, oturum sonunu
saymamak, kovayı sınırsız doldurmak, düşeni saymamak; yazıcıda hiç
kuyruklamamak, açıkken kuyruklamak, sonda terk etmemek, raporu/sessizliği
denetleyiciye vermemek, kadansı izlememek, kapalıyken hızlamak, girişte
kovayı yeniden başlatmamak, kesintide kırpmamak, hızlama son tarihini
`min`'e katmamak; B91 geri çekilmesi yok, sessizlik sıfırlanmıyor,
pencere dönmüyor, pencere sessizliği silmiyor, baytlar sayılmıyor; B96:
`close`'ta izleyiciyi kesmemek. **Sağ kalan:** yeniden gönderilen REL
karesinin kovadan düşülmemesi (hızlıyken yeniden gönderim nadir;
ilk gönderimin düşülmesi test ediliyor) — bilinçli bırakıldı.

**Gecikme sinyali, artış ve taban — tur 4 (B104 ölçümü, 2026-10-03).**
Bakımcı `scripts/rudp-jitter.sh`'ı koştu: 64 istemci × 30 sn × her
senaryo×kip 3 koşu, `orchestrate`, loopback'te netem (gecikmeler yön
başına), darboğaz `rate 3611kbit limit 1000` (baseline'ın ölçtüğü talebin
yarısı); makine `HOME`, çekirdek 7.2.6-arch2-1, başlangıç
2026-10-03T02:15:29+03:00. Kod: tur 3 (`main` @ c5e8c55 hattı).
`summary.txt`:

```text
scenario           mode runs episodes     cuts   dropped    queued   lost%   rtt_ms  conn_p99   p99_max    snaps/s  ends
baseline           off     3      0.0      0.0       0.0       0.0    0.00      0.2      31.0        60     1905.0     0
baseline           pace    3      0.0      0.0       0.0       0.0    0.00      0.2      61.3        62     1905.0     0
jitter20           off     3      0.0      0.0       0.0       0.0    0.18     40.2     129.7       135     1908.3     0
jitter20           pace    3    135.7    240.0     162.3    3286.7    0.19     40.1     122.7       127     1902.6     0
jitter40           off     3      0.0      0.0       0.0       0.0    0.24     80.6     247.3       267     1904.0     0
jitter40           pace    3    327.0   1815.0    7063.7   29964.7    0.28     80.6     246.3       260     1669.0     0
jitter60           off     3      0.0      0.0       0.0       0.0    0.35    121.4     417.0       452     1895.9     0
jitter60           pace    3    176.0   2996.3   19262.3   51369.7    0.45    122.2     414.7       429     1255.2     0
jitter40_loss1     off     3      0.0      0.0       0.0       0.0    1.13     80.4     264.0       280     1884.3     0
jitter40_loss1     pace    3    298.0   1946.3    8363.7   32479.0    1.16     80.1     260.3       291     1608.9     0
bottleneck         off     3      0.0      0.0       0.0       0.0   50.97   1438.1     132.7        67      754.2    38
bottleneck         pace    3    100.0   1639.3   12022.0   26160.3   37.11   1382.3     163.3       201      733.1    10
bottleneck_jitter  off     3      0.0      0.0       0.0       0.0   45.79   1417.5     193.7       203      749.0     0
bottleneck_jitter  pace    3    101.0   1670.3   12125.3   26604.7   40.55   1411.2     171.7       179      732.0     0
```

*Teşhis (doğrulandı).* (1) **Titreşimde sahte gecikme sinyali.** Tur 3'ün
kuralı tek örnekti: en yeni tur pencereli en küçüğün 30 ms üstündeyse
sinyal. Pencerenin en küçüğü saniyelerin en şanslı örneğidir (σ'lık
titreşimde ortalamanın ~2σ altı); σ ≈ 28 ms'lik turda (jitter40) tek
örnek 30 ms'yi örneklerin yarısından fazlasında aşar, iki ardışık sinyal
de sıradandır — 64 oturumda koşu başına 327 bölüm, 1815 kesinti,
snapshot'ların %12'si gitti (jitter60: %34). Kayıplı senaryo jitter40'ın
aynısı: %1 rastgele kayıp `LOSS_MIN`/`LOSS_DIV`'in altında, suçlu gecikme
sinyali. (2) **Darboğazda taban yolun kendisiydi.** Taban 4 bütçe/sn =
5 888 B/sn; 64 oturumda 377 KB/sn yük ≈ 3,0 Mbit/sn — loopback başlıklarıyla
(~470 B'lik snapshot + 42 B) ve hızlı kadanstaki sonda/raporlarla 3,6
Mbit/sn'lik bağlantının ~%97'si: hızlama bağlantının altına inemezdi,
netem kuyruğu (1000 paket, ~0,7 sn yön başına; sonda turu ~1,4 sn) hiç
boşalmadı. (3) **Artış 64 oturuma göre değildi:** 16 bütçe/sn² = 23,5
KB/sn² oturum başına; 250 ms'lik raporda +5,9 KB/sn × 64 = +376 KB/sn —
bağlantının %83'ü her temiz raporda. Çevrimdışı bir modelle (netem'in
tfifo'su — iki yön tek kuyruk, hız ve paket sınırı —, sondalar, raporlar,
`feedback`'in aralık hesabı ve denetleyici birebir; repo'da değil) ölçüm
yeniden üretildi (jitter40 `pace`: model 291 bölüm/1991 kesinti/9547
düşen, ölçüm 327/1815/7064; jitter60 149/3084/20 994 — 176/2996/19 262;
darboğaz 144/2008 — 100/1639) ve ayrıştırıldı: darboğazda tur 3 → oyun
yaşı p50 963 ms; yalnız taban 1 bütçe → 222 ms; taban + talebe göre artış
→ 35 ms. İkisi de gerekliydi.

*Yeni gecikme sinyali (`udp::congestion::signal`, `Control::on_estimate`).*

- **Kuyruk bir en küçüktür.** Ayakta kuyruk = en yeni turların küçüğü
  pencereli tabanın eşik kadar üstünde (LEDBAT'ın "current delay"
  süzgeci, RFC 6817 §3.4.2, BBR'nin pencereli tabanına karşı): titreşim
  bir örneği yükseltir, ardışık örneklerin küçüğünü yükseltmez; ayakta
  kuyruk hepsini yükseltir. Şüpheden hızlamaya: en yeni **2** tur;
  hızlanırken: en yeni **1** tur (yol zaten tıkalı biliniyor — tepki
  anında; iki örnekle beklemek röle testinde p95'i ~100 ms büyüttü).
- **Eşik yolun titreşimidir:** `clamp(5 × titreşim, 30 ms, 300 ms)`.
  Gerekçe: σ'lık titreşimde pencerenin tabanı ortalamanın ~2σ altında;
  taban + 5σ ortalamanın ~3σ üstü — tek örnekte ~1/740, ardışık iki
  örnekte ~2·10⁻⁶; titreşim tahmininin kendi hatasına (genç tahminde
  %±30) pay bırakır. Titreşimsiz yolda eşik tur 3'ün 30 ms'si; B104'ün en
  sert senaryosunda (yön başına σ 40 ms → tur σ ≈ 55 ms) eşik ~275 ms.
  300 ms tavan: hiçbir titreşim o kadar ayakta kuyruğu gizlemez ve o kuyruk
  titreşim bilinmeden de hızlandırır (darboğazın ilk saniyeleri).
- **Titreşim, bir parabolün izleyemediğidir.** En yeni hızlı örneğin,
  kendinden önceki üç örnekten geçen parabolden (Lagrange — eşit olmayan
  kadans da eğilim değildir) uzaklığı, dış-değerlemenin taşıdığı gürültüyle
  ölçeklenmiş: normal titreşimde beklenen değeri σ. Sabit hızla dolan,
  boşalan ya da hızlanarak dolan (hızlanan oturumun kendi toplamsal artışı)
  kuyruk ona hiçbir şey katmaz. **Elenen: doğru (ikinci fark)** — hızlanan
  oturumun kendi kuyruğu eğridir, doğru onu titreşim sandı; derin tamponda
  eşik 55–65 ms'ye çıktı, oyun bandı p95 ~265 ms. **Yalnız hızlı kadans
  (250 ms) öğretir:** saniyede bir örnekte, başka oturumların hızlamasının
  birkaç saniyede bir yükseltip indirdiği kuyruk titreşime benzer —
  geç katılan bir oturum onu titreşim sanıp bağlantıyı bırakmadı (model,
  röle paylaşımlı: Jain 0,71). **Elenen: yavaş kadansta yalnız düşürme** —
  aşağı yönlü yanlı (örnekler hep tahminin altında sayılır): jitter
  modelinde 7 kat fazla sahte bölüm. Her uzaklık en çok 3 × tahmin
  sayılır (en az eşiği 30 ms olan titreşim, 6 ms; tahmin gençken — ilk
  16 uzaklık — en az eşiği 300 ms olan, 60 ms): bir kesintinin köşesi
  sakin yolun tahminini şişirmez, titreşimli yolun genç tahmini birkaç
  örnekte titreşimine büyür. İlk 16 uzaklığın ortalaması, sonra 1/16
  kazanç (RFC 3550'nin interarrival jitter kazancı).
- **Şüphe ucuzdur, karar bilgi ister.** `Open` → `Suspect`: en yeni tur
  tabanın 30 ms üstünde (sabit, titreşimden bağımsız) ya da kayıp
  sinyali — yalnız sondalar hızlanır (36 B/sn), hızlı örnekler titreşim
  tahmininin girdisidir. `Suspect` → `Paced`: üst üste iki kayıp sinyali;
  ya da ayakta kuyruk **ve** tahmin en az 4 hızlı uzaklığa dayanıyor (ya
  da kuyruk ≥ 300 ms). Ayakta ama bilinmeyen kuyruk şüpheyi sürdürür
  (öğrenir); 4 raporda ayakta kuyruk yoksa `Open`. **Elenen: girişte 4
  örnek (bir saniye) ayakta kuyruk ya da 8 uzaklıklık bilgi kapısı** —
  titreşime en dayanıklılarıydı ama hızlananların 250 ms'de söndürdüğü
  kuyruğu geç katılan göremedi ya da geç öğrendi: röle paylaşımlı
  testinin ara koşularında son katılan sık sık açık kaldı (Jain 0,71–0,95).
- **Boşalma tutar:** hızlanırken en yeni tur 5 ms'den çok düştüyse
  (`DRAIN_MIN`) kesinti çalışıyordur — hız ne kesilir ne artar (tur 3:
  yalnız gecikme kesintisini tutuyordu; artışı da tutmak, kuyruk
  boşalmadan yeniden doldurmayı önler — model, ara tasarım: darboğaz
  yaşı 378 → 60 ms).
  5 ms'lik pay: kuyruğun kuyruğundaki ms'lik düşüşler tutmayı sonsuza
  uzatmasın (0 payla derin tamponda hız boş bağlantıda kaldı).
- **Alım süresi** kuyruğun büyümesiyle uzar — tek örneğin değil en yeni
  iki turun küçüğünün büyümesiyle: titreşimli bir örnek teslim edilen
  hızı yarıya indirip yanlış bölümde derin kesmesin.

*Artış ve taban.* **Artış = bölümün en yüksek talebinin ¼'ü/sn**
(`INCREASE`): röle testinde (91 KB/sn talep) tur 3'ün 23,5 KB/sn²'si
(0,26 × talep) — orada sınanmış dinamik aynı kalır; B104 yükünde (oturum
başına ~14 KB/sn) 3,5 KB/sn², 64 oturumda raporda +56 KB/sn — bağlantının
%12'si (tur 3: %83). Toplamsal (eşit taleplerde eşit adım: Chiu–Jain
yakınsaması korunur); en yüksek talep, içeriğini bütçeye inceltten odanın
kendi toparlanmasını yavaşlatmaz ve gürültülü bir aralık oturumu erken
açmaz (açılma o talebin 1,25 katında). **Elenen: ⅛** — darboğazda kuyruğu
daha kısa tuttu ama röle paylaşımlı testinde (ara tasarım) yakınsama 40
sn'ye sığmadı (Jain 0,95). **Taban = 1 bütçe/sn** (1472 B/sn): en yeni kare yine de
istemciye ulaşır (en çok saniyede bir datagram), teslim ölçümü sürer; 64
oturumun tabanı 94 KB/sn = 0,75 Mbit/sn, B104 bağlantısının %21'i.
**Elenen: talebe göre taban** — sabit bir bütçe yeter ve açıklanması
kolay; aynı darboğazı N oturum paylaştıkça her taban bir gün bağlantıyı
aşar, 1 bütçe bunu 3,6 Mbit'te ~300 oturuma iter.

*Testler (önce kırmızı; her kural mutasyonla).* `udp::congestion::tests`
(10), `…::tests::signal` (6: en küçük, düz/hızlanan kuyruk titreşim
değildir, normal titreşim σ'dır, yavaş kadans öğretmez, köşe en çok
kırpma kadar sayar, kenetler), `…::tests::path` (3, deterministik —
tohumlu üreteç netem'in `delay ort sd distribution normal`'ını yön başına
çeker; pencere tabanı son 10 sn'nin en küçüğü, iki kovalınınkinden
katı): **(a)** `jitter_alone_paces_rarely` — B104'ün üç titreşimi × 30
tohum × 10 dk, kayıp yok: 300 oturum-dakikada 10/8/7 bölüm, 25/21/17
kesinti (sınır: ≤ 12 bölüm, bölüm başına ≤ 3 kesinti; tur 3 B104'te 32
oturum-dakikada 136–327 bölüm); **(b)** `a_standing_queue_paces_within_bounded_reports`
— 60 sn yoldan sonra kalıcı kuyruk: hiç şüphelenilmemiş sakin yolda en
çok 3 + 4 raporda, titreşimi bilinen yolda 2'de, 350 ms'lik kuyrukta en
sert titreşimde 2'de; **(c)** `the_floor_lets_a_shared_link_drain` — 64
oturum, bağlantı oturum başına 2 bütçe/sn, talep 10, 1 sn'lik kuyruk
sonu düşüren tampon: kuyruk boşalır, son 20 sn'de ortalama < 100 ms,
bağlantı > %50 dolu, Jain > 0,9. Gerçek sokette `udp::tests::pace`
(taban/hız: teslim × β, 1 bütçe altına inmez). **Önce kırmızı:** tur 3'ün
gecikme kuralı (eşik sabit 30 ms) (a)'yı ve (b)'yi, tur 3'ün tabanı (4)
ve sabit artışı (c)'yi düşürür. **Mutasyonlar 27'de 25 öldü, kalan ikisi
için test eklendi ve öldüler** (yedekle–boz–koş–geri yükle): titreşimsiz
eşik, tavansız eşik, çarpan 2, yavaş kadans öğretir, kırpma yok, genç
kırpma dar, ortalama yok, ardışık fark, ölçeksiz uzaklık, girişte tek
örnek, hızlanırken iki örnek, bilgi kapısı yok, 300 ms'de beklemesiz giriş
yok, tek kayıp hızlandırır (eklendi), şüphe hiç açılmaz, şüphe ayaktayken
açılır, ipucu titreşim eşiğinde, boşalma tutmaz, boşalmada artar, boşalma
payı 0 (eklendi), artış/açılma anlık talebe göre, alım süresi uzaması
yok, taban 4, sabit artış, yeni yol örnekleri tutar, tur 3'ün kuralı.

*Röle ölçümü önce/sonra* (`udp::tests::pace::measure`, `--ignored`, aynı
makine, 2026-10-03; tur 3 kodu bir, tur 4 kodu iki koşu; `pace`; `off`
satırları değişmedi).

| senaryo | kod | mesaj/sn | oyun yaşı p50 / p95 / max ms | kontrol yaşı p50 / p95 ms | kesinti | teslim | Jain |
|---|---|---|---|---|---|---|---|
| derin | tur 3 | 19,2 | 97 / 147 / 182 | 44 / 104 | 11 | 816 KB | — |
| derin | tur 4 | 19,0; 18,1 | 107–114 / 211–268 / 257–290 | 52–55 / 171–225 | 8–9 | 759; 735 KB | — |
| sığ | tur 3 | 19,2 | 97 / 131 / 153 | 40 / 75 | 12 | 816 KB | — |
| sığ | tur 4 | 19,0; 19,1 | 101–103 / 149–153 / 168–169 | 44–45 / 92–103 | 8 | 797; 795 KB | — |
| paylaşımlı (oturum başına) | tur 3 | 13,5–16,0 | 99–103 / 139–150 / ≤164 | 44–46 / 79–84 | 160 | 7,05 MB | 0,996 |
| paylaşımlı (oturum başına) | tur 4 | 9,7–16,3 | 98–126 / 170–207 / ≤231 | 38–43 / 112–141 | 119–120 | 6,76; 6,79 MB | 0,995; 0,967 |
| darboğazsız | ikisi | 30,5 | 21–22 / 22–23 | 21–22 / 22–23 | 0 | 915 414 B | — |

**Okuma: gerileme var, bilinçli.** Tur 4 adil (Jain 0,967–0,999, ara
koşularda 4/4 oturum hızlandı) ve titreşime dayanıklı; ama derin tamponda
p95 +60–120 ms, paylaşımlıda +30–60 ms, teslim −%4…−%10. Neden: hızlanan
oturumun kendi testere dişinin köşeleri titreşim tahminini 6–12 ms'ye
taşır, eşik 30 yerine 30–60 ms olur — tur 3'ün sabit 30 ms'si ise
titreşimli yolda yanlış alarmdı. Koşudan koşuya oynama da büyük (derin
p95 tur 4 ara koşularında 145–268). Yeni satır (BACKLOG): eşiğin
hızlanırken kendi testere dişinden arındırılması.

*Yeniden ölçüm (bakımcı) ve geçme ölçütü.* `scripts/rudp-jitter.sh`
olduğu gibi (64 istemci, 30 sn, 3 koşu); `summary.txt`'nin sonuna
`verdict` bölümü eklendi (`scripts/README.md`): `jitter*` — `pace`'in
snapshot/sn'si `off`'un ≥ %98'i, hızlamanın düşürdüğü ≤ teslim edilen
snapshot'ların %1'i, istemci başına koşuda ≤ 1 kesinti; `bottleneck*` —
bölüm var, `lost%` ≤ `off`'un yarısı, `rtt_ms` ≤ üçte biri, `ends` ≤
`off`'unki. Hepsi geçerse (`overall: PASS`) varsayılan `"pace"` olabilir.
Tur 3'ün verisi 6/6 KALIR. Model tahmini (ölçüm değil): titreşimde koşu
başına ~8 bölüm, 22–30 kesinti, 180–440 düşen kare, snapshot −%0,4…−%0,9;
darboğazda `lost%` ~%17 (`off` ~%51), `rtt_ms` 210–285 (`off` ~1970).

**Bağlantı göçü (BACKLOG B3 — 2026-10-02, opt-in; bakımcı kararları
RUDP-SECURITY §2 #5 ve #10).** Eskiden demux oturumu istemcinin
4-tuple'ıyla anahtarlıyordu: NAT yeniden bağlanması (yeni kaynak portu)
ya da Wi-Fi ↔ hücresel geçiş (yeni adres) YENİ bir oturumdu — yeni el
sıkışma, aynı kimlikle resume (RECONNECT §5). Bakımcı kuralı: mobil
oyuncu adresini sürekli değiştirir, bunun için oturum kapatmak yanlış.
Artık oturum adresten bağımsız bir **bağlantı kimliği (CID)** taşıyabilir
ve yol doğrulamasından sonra yeni adrese **göçer**. Kod: `udp::path`
(kurallar ve saf durum), `udp::demux::{table, dispatch, migrate}`,
`udp::writer::path`, `udp::client::migrate`.

*Opt-in (karar 5).* Kriptodan önce CID **taşıyıcı jetondur**: istemcinin
trafiğini koklayan biri CID'yi okur, düz metin PATH_CHALLENGE'ı kendi
adresinden yanıtlar ve oturumun s→c akışını kendine çeker. Bu yüzden
düz metin kapı CID'yi yalnız `udp_migration = true` ise verir; orada
varsayılan kapalı ve kapı bayt bayt eskisidir (test, aşağıda). B5a'dan
beri mühürlü kapıda (sunucunun varsayılanı) challenge şifrelidir ve
göç varsayılan açıktır (B112; aşağıda "Kayıt katmanı").

*Tel — hepsi eklemeli (§5'in evrim kuralı):*

| Datagram | Bayt düzeni | Boy | Not |
|---|---|---|---|
| proof | `[3][u64 nonce][u64 cookie][0][u8 caps]` | 19 B | bayt 17 HELLO'nun hep sıfır olan dolgusu (değişmedi); caps bayt 18'e EKLENİR; bit 0 = `CAP_CID` ("bana CID ver"). Challenge isteği değişmedi (18 B) |
| accept | `[2][u32 1][u64 cid]` | 13 B | `ACK{1}` + CID; yeniden gönderilen proof'a AYNI CID (idempotent). CID yoksa 5 B'lik eski accept |
| etiketli c→s | `[k \| 0x80][u64 cid][k türünün gövdesi]` | +8 B | CID verildikten sonra istemcinin HER datagram'ı (RAW, REL, ACK, REPORT); s→c asla etiketlenmez |
| PATH_CHALLENGE | `[7][u64 nonce]` | 9 B | yalnız s→c, aday adrese |
| PATH_RESPONSE | `[0x88][u64 cid][u64 nonce]` | 17 B | yalnız c→s, her zaman etiketli |

*Kind bayt haritası (kesin; B109 kapandı):* `0x00..=0x3F` düz metin
türler (`0..=8` kullanımda); `0x40..=0x7F` SEALED kayıt
(`crate::seal`, `0x40 | anahtar fazı`; B5a'ya dek bağlı değil);
`0x80..=0xBF` CID etiketli düz metin tür (yalnız c→s);
`0xC0..=0xFF` boş. SEALED c→s kaydı CID'yi kendi başlığında aynı
konumda (bayt 1..9) taşıdığı için etiket bitini almaz; demux iki
biçimde de CID'yi aynı yerden okur. Aralıkların ayrıklığı ve CID
konumunun eşitliği derleme zamanı `assert`'leridir (`udp/mod.rs`).

*Kurallar (RFC 9000 §9 ve karar 10):*
- **Yönlendirme.** Etiketsiz datagram adresle (eskisi gibi), etiketli
  datagram CID ile bulunur. Bilinmeyen CID düşer, sayılır
  (`udp_cid_unknown`). Göç kapalı kapıda etiketli datagram bilinmeyen
  türdür: `udp_datagrams_malformed`, oturuma dokunulmaz (eski kod yolu).
- **Yeni adresten etiketli datagram** bir doğrulama başlatır: sunucu o
  adrese taze rastgele nonce'lu `PATH_CHALLENGE` yollar ve **diğer her
  şeyi eski yolda göndermeye devam eder** (karar 10 — doğrulanmamış
  adrese erken gönderim yok). Challenge en çok `CHALLENGE_RESEND`
  (200 ms = el sıkışma adımının tavanı) aralıkla, yalnız aday yeniden
  konuşunca, ve adaydan alınan baytların 3 katını aşmadan yeniden gider.
- **Doğrulanmamış adresten gelen girdi KABUL edilir** (oyun, kontrol,
  ACK, rapor). Gerekçe: RFC 9000 §9 bu paketleri işler; kriptodan önce
  yeni adresten gelen datagram eskiden gelen kadar güvenilirdir (hiç —
  eski adresi sahtelemek de aynı enjeksiyonu yapar), düşürmek ise her
  dürüst yeniden bağlanmaya bir RTT'lik girdi kaybettirirdi. Demux'ın
  cevapları (REL'in ACK'i) yine eski, doğrulanmış adrese gider.
- **Eşleşen `PATH_RESPONSE`** (bekleyen nonce, aday adresten) oturumu
  taşır: adres indeksi yeni adrese geçer, yazıcıya çıkış kanalından
  `UDP_PATH` (op 14, tele çıkmaz) bildirimi gider — piggyback ACK gibi:
  yazıcının tek beklenen kaynağı aynı kalır, kilit yok. Bildirimden önce
  kuyruğa giren kareler eski adrese, sonrakiler yeni adrese. Bağlantı
  aktörüne de gelen kutusundan `ConnIn::PeerChanged` gider (B113,
  aşağıda): ikisi birlikte ya da hiçbiri — demux önce iki kanalda da yer
  ayırır (`try_reserve`). Kanallardan biri doluysa taşıma yapılmaz
  (sayılır), doğrulama bekler, bir sonraki challenge turu yeniden dener.
- **Eski adres göç bitene dek çalışır.** Göçten sonra eski adres
  kimsenin değildir; oradan etiketli bir artık datagram CID ile yine
  oturumundur ve yeni bir aday sayılır.
- **Doğrulama tek adla biter:** göç (`udp_migrations`), zaman aşımı
  (`VALIDATION_TIMEOUT` = 3 sn — RFC 9000 §8.2.4'ün yeni yolun ilk
  RTT'siyle üç PTO'su; REL canlılık sınırının (5 sn) altında, ki eski
  yolu ölmüş istemci bandı ölmeden göçsün; oturumun bir sonraki
  datagram'ında ya da sonunda fark edilir), yerine yenisinin gelmesi
  (üçüncü bir adresten etiketli datagram: en yeni aday kazanır — RFC
  9000 §9.3'ün "en yüksek numaralı sondalamayan paket" kuralı, B5a'ya
  dek numarasız), oturum biterken açık. Defter:
  `started = migrations + timed_out + superseded + open_at_end` (+ hâlâ
  bekleyenler).
- **Başka oturumun adresi aday olamaz** (`udp_path_address_in_use`):
  bir adres, bir oturum.
- **CID:** 64 bit, `getrandom` (0.4.3, cookie key'in kullandığı sürüm)
  ile oturum başına çekilir; `ConnectionId`'nin sırası değil, tahmin
  edilemez. Entropi başarısızsa (ya da 2^64'te bir çakışırsa) oturum
  CID'siz kurulur — taşınamaz ama çalışır, sayılır
  (`udp_entropy_draws_failed`); zayıf değerle asla.
- **Yol tahmini (RFC 9000 §9.4):** yeni IP'de yazıcı REL bandının RTT
  tahmincisini (`Rto::default`), oyun bandının pencereli RTT'sini ve
  tahminini (`Feedback::new_path`) ve tıkanıklık denetimini (açık,
  hızlanmamış — kuyruktakiler bir sonraki geçişte gider) sıfırlar; yalnız
  port değişimi (NAT yeniden bağlanması — aynı yol) tahmini korur.
  Yoldaki sondalar ve aralık temeli korunur: rapor yine sondasını anar,
  gönderilen her datagram bir kez sayılır.

*Demux'ın tablosu.* Oturumlar iç anahtarla (`SessionKey`, asla yeniden
kullanılmaz, tele çıkmaz) tutulur; iki indeks: `addr → key`, `cid → key`.
Deadline kümesi `(Instant, SessionKey)`, reap kuyruğu da anahtar taşır:
göç ne deadline'ı ne reap sinyalini taşımak zorundadır. c2'nin geç
deadline kuralı aynen çalışır (girdi yine `last_seen + idle` ile
karşılaştırılır; aynı adrese gelen yeni oturumun anahtarı farklı olduğu
için eskisinin girdileri doğrudan bayattır). Datagram başına maliyet bir
hash araması artar (adres ya da CID → anahtar → oturum).

*İstemci.* `UdpClientConfig::migration` (varsayılan AÇIK: proof'a bir
bayt; eski ya da göçü kapalı sunucu yok sayar ve CID vermez, istemci de
hiç etiketlemez). CID'yi yalnız istediği accept'ten alır; sonra her
datagram'ı etiketler (REL'in sakladığı ve yeniden gönderdiği datagram
da etiketli). Challenge'ı anında, etiketli yankılar.
`UdpClient::rebind()`: yeni yerel soket (Wi-Fi ↔ hücresel), aynı
oturum; eski soket kapanır, yeni soketten hemen etiketli kümülatif ACK
gider ki sunucu bir sonraki kareyi beklemeden doğrulamaya başlasın. CID
yoksa `Unsupported` döner ve hiçbir şey değişmez (çağıran reconnect +
resume yapar). NAT yeniden bağlanması için istemci hiçbir şey yapmaz:
onu göremez, her datagram'ı oturumunu CID ile anar. Sayaçlar
(`UdpClientStats`): `rebinds`, `path_challenges_answered`,
`path_responses_send_failed`, `path_challenges_ignored`.

*Uyumluluk matrisi (her iddia kodda ve testte doğrulandı).*
- **Yeni istemci → eski sunucu:** eski `handle_hello` proof'un yalnız
  1..17 baytlarını okur (`n < 18` kontrolü, sondaki bayt yok sayılır),
  5 B accept yollar; istemci CID almaz, hiç etiketlemez, `rebind`
  `Unsupported`. Göçü kapalı yeni sunucu aynı yoldadır (test).
- **Eski istemci → yeni sunucu:** caps yok → CID yok → 5 B accept, etiket
  yok, tel bayt bayt eskisi (test). Eski istemci accept'in yalnız 1..5
  baytlarını okur (`d.len() < 5` kontrolü), sondaki CID'yi yok sayardı;
  zaten istemeyen istemciye CID gitmez.
- **Eski istemci → eski sunucu:** değişmedi.
- **Yeni ↔ yeni, göç açık:** göç (test).
- Eski istemci NAT arkasında yeniden bağlanırsa geri düşüş yolu: yeni
  adresinden gelen datagram'ları oturumsuz sayılır
  (`udp_datagrams_no_session`), resume'la döner (test).

*Amplifikasyon.* 3× bütçe kuralı uygulanır ve sayılır
(`udp_path_amplification_capped`), ama bugünkü boylarla **bağlamaz**:
aday ancak ≥ 9 B'lik etiketli bir datagram'la doğar ve her challenge
9 B, yeniden gönderim de yalnız adaydan yeni bir datagram gelince olur
— oran yapısal olarak ≤ 1. Kural B5a'nın mühürlü boyları ya da ileride
bir PMTU dolgusu için oradadır; saf durumda ve demux'ta testle
kilitli. Challenge dolgulanmaz (QUIC'in 1200 B'si gibi): rUDP'nin
datagram bütçesi kapı genelinde tek değerdir, yola göre keşfedilmez.

*Sayaçlar (taşıma kapsamı, OPS §3; tablonun sonuna eklendi):*
`udp_cids_assigned`, `udp_entropy_draws_failed`, `udp_cid_unknown`,
`udp_path_validations_started`, `udp_path_challenges_sent`,
`udp_path_challenges_send_failed`, `udp_path_amplification_capped`,
`udp_path_address_in_use`, `udp_path_responses_unmatched`,
`udp_path_changes_not_forwarded`, `udp_path_validations_timed_out`,
`udp_path_validations_superseded`, `udp_path_validations_open_at_end`,
`udp_migrations`, `udp_migrations_port_only`. (Brifteki
`udp_paths_validated` ayrı bir sayaç değil: eşleşen yanıt ya taşır
(`udp_migrations`) ya yazıcıya (B113'ten beri: ya da aktöre) ulaşamaz (`udp_path_changes_not_forwarded`,
doğrulama bekler) — iki ad, iki anlam, B32'nin kuralı.) Yazıcının
oturum log'unda `path_changes`, `path_resets`.

*Elenenler.* (1) *Göçü varsayılan açmak* — karar 5: kriptosuz CID
taşıyıcı jeton. (2) *Doğrulanmamış adrese erken gönderim* (yeni adresi
hemen kullanmak) — karar 10; bedeli ~1 RTT oyun bandı kaybı, REL zaten
yeniden gönderir. (3) *Doğrulanmamış adresten gelen girdiyi düşürmek* —
yukarıda: güvenlik kazancı yok, her dürüst göçe bir RTT girdi kaybı.
(4) *Oturumları yeni adrese yeniden anahtarlamak (tek indeks, deadline'ı
taşımak)* — reap sinyali ve geç deadline girdileri adresi izlemek
zorunda kalırdı; iç anahtar ikisini de adresten bağımsız yapar. (5)
*Her ACK'e CID eklemek* (accept kaybına karşı) — oturum başına her
ACK'te 8 B; accept kaybolursa istemci proof'u yeniden yollar ve yeniden
cevaplanan accept aynı CID'yi taşır. (6) *Challenge'ı zamanlayıcıyla
yeniden göndermek* — demux'a ikinci beklenen kaynak ya da tarama; aday
konuştukça yeniden göndermek yeter. (7) *`ConnectionId`'yi CID olarak
kullanmak* — sıralı, tahmin edilebilir.

*Testler (önce kırmızı; mutasyonlu).* `udp::path::tests` (saf kurallar:
challenge aralığı, 3× bütçe, reddedilen challenge bütçe yemez,
nonce+adres eşleşmesi, zaman aşımı sınırı, `UDP_PATH` yükü),
`udp::demux::tests::grant` (CID yalnız göç açık + istenmişse; 13 B
accept; yeniden proof'a aynı CID; iki oturum, ilgisiz iki CID; göç kapalı
ya da eski istemci → 5 B accept; göç kapalı kapıda etiketli datagram
bilinmeyen tür), `udp::demux::tests::migrate` (kendi adresinden etiketli
datagram yönlenir; bilinmeyen CID sayılır; tam göç: B'den girdi kabul,
B'ye yalnız challenge, ACK A'ya, A çalışır, eşleşen yanıt taşır ve
yazıcıya söyler; sahte kaynak yanıtlamaz → zaman aşımı, eski yol
etkilenmez, yanlış nonce/adres eşleşmez; en yeni aday kazanır, başka
oturumun adresi reddedilir; `limits`: 3× bütçe, dolu yazıcı kanalı
taşımayı erteler, oturum sonu defteri, istemcinin göndermediği etiketli
türler, yeni IP port-only değildir), `udp::writer::tests::path` (yeni
port: gönderimler yeni adrese, tahmin korunur; yeni IP: RTT, oyun
tahmini ve tıkanıklık sıfırlanır; bildirim oturum karesi sayılmaz),
`udp::client::tests::migrate` (CID yalnız istenen accept'ten; sonrası
hep etiketli — yeniden gönderim, rapor, ACK dahil; challenge yankısı;
`rebind` soketi taşır, CID'siz reddeder), `udp::tests::migrate` (gerçek
soket: istemcinin görmediği NAT yeniden bağlanması — iki bant iki yön
akar, yeni el sıkışma yok, `udp_migrations = 1`, port-only; `rebind`;
matrisin diğer köşeleri; `identity`: göç kapalı kapı, caps'li ya da
caps'siz proof ve göç açık kapıda eski istemci — sunucunun her baytı
aynı ve eski telin baytları), `gsb-server/tests/rudp_resume`
(`a_migrating_rudp_session_survives_its_address_change`: el sıkışmasız,
resume'suz, `closes 0`, registry `opens 1`; göç açıkken kaybolan
istemci yine resume eder; yedi geri düşüş testi değişmeden yeşil).
Testler uygulamanın iskeletinden sonra yazıldı; kırmızıları her kuralı
tek tek bozarak gösterildi: **31 mutasyonun 31'i öldü** (her biri dosya
karalama dizinine yedeklenip bozuldu, koşuldu, geri yüklendi):
etiketli yönlendirmeyi kapatmak, challenge göndermemek, nonce'u ya da
adresi denetlememek, yazıcıya söylemeden taşımak, doğrulamadan önce
taşımak (karar 10), 3× bütçeyi kaldırmak, challenge'ı her datagram'da
yollamak, zaman aşımını kaldırmak, bekleyeni değiştirmemek, başka
oturumun adresini kabul, oturum sonunu deftere yazmamak, CID'yi göç
kapalıyken ya da istenmeden vermek, yeniden proof'a CID'siz cevap,
sıralı CID, istemcinin istemediği CID'yi alması, istemcinin
etiketlememesi, challenge'ı yanıtlamaması, yazıcının eski adreste
kalması, port değişiminde sıfırlamak, IP değişiminde sıfırlamamak
(REL, oyun tahmini, tıkanıklık — üçü ayrı ayrı), bilinmeyen CID'yi
saymamak, bildirimi oturum karesi saymak, port-only'yi hep saymak,
`rebind`'in dürtmesini kaldırmak, caps baytını yanlış konumdan okumak,
sunucunun `udp_migration`'ı kapıya iletmemesi (`rudp_resume`).

*Göçte kaynak (B113 — u89, 2026-10-02).* Göç artık oturumun kaynağını
taşır; üç yerde:
- **Aktör:** `ConnIn::PeerChanged { peer }` (enum'un sonunda) `peer`'i
  günceller (kapanış sinyalleri yeni adresi adlandırır) ve registry'ye
  `RegistryMsg::ConnPeerChanged { conn, source }` yollar — `Authed`'in
  kullandığı gönderimle, ikisi sırasını korur. Bildirim kareleri de
  taşıyan gelen kutusundan geçer: göçten önceki kareler önce işlenir.
- **Registry (D12):** satırın kaynağı yeni kaynağa geçer; unauthed
  satırın kaynak başına sayımı onunla taşınır. **Yeni kaynak sınırını
  zaten tutuyorsa** sayım eski kaynakta kalır ve sayılır
  (`unauth_source_moves_kept`); taşıma (taşımanınki) geri alınmaz.
  Authed satır hiçbir sayımda değil, sadece izler.
- **Demux (B89):** oturum hâlâ bekleyense (kurulmuş, accept döngüsü
  almamış) sınırdaki yeri aynı kuralla taşınır
  (`udp_pending_source_moves_kept`).

*Neden "taşı ya da yerinde bırak", "reddet" ya da "serbest bırak"
değil.* Göçü reddetmek (eski yolda kalmak) NAT'ı yeniden bağlanan ya da
dolu bir CGNAT'a geçen dürüst oyuncuyu ölü yolda bırakırdı — demux
registry'nin sayımını bilmez de (bilmek için bekleme gerekirdi; demux
beklemez). Sayımı sınıra bakmadan taşımak sınırı delerdi: iki adresli
saldırgan oturumları A'dan doldurup B'ye taşır, A boşalır, yeniden
doldurur — B sınırsız büyür. "Yerinde bırak" ikisini de önler: her
sayım oturumun kanıtladığı (dönüş yolu olan) bir kaynakta durur, hiçbir
kaynak sınırı aşmaz, havuz `U` hâlâ `U`/sınır kaynakla dolar; dürüst
oyuncu göçer ve AUTH'la sayımdan çıkar. Unauthed pencere bir AUTH
gidiş-dönüşüdür; bu durum nadirdir, ama sayılır.

*B5a'da yapıldı* (aşağıda "Kayıt katmanı"): CID msg2'nin şifreli
yükünde; etiketli düz metin yerine SEALED c→s (CID aynı konumda); göç
kuralının üç koşulu; PATH_* şifreli iç tür; mühürlü kapıda
`udp_migration` varsayılanı açık.

**Kayıt katmanı — mühürlü kapı (BACKLOG B5a — 2026-10-02; tasarım ve
tehdit modeli `docs/RUDP-SECURITY.md`; bakımcı kararları 3, 5, 6).**
`udp_security = "sealed"` (varsayılan) kapısında `gsb_net::seal`'in
sans-IO çekirdeği rUDP'ye bağlıdır: Noise `NK_25519_ChaChaPoly_BLAKE2s`
çerez el sıkışmasına biner (0 ek RTT), sonra oturumun her datagramı iki
yönde SEALED kayıttır. `"plaintext"` kapı B5a öncesinin bayt bayt
aynısıdır (dev/LAN; başlangıçta tek `warn`). Kod: `udp::sealed` (kip,
el sıkışma teli, DH bütçesi, `UDP_SEND`), `udp::demux::{noise, record}`,
`udp::writer::seal`, `udp::client::seal`.

*Tel:*

| Datagram | Bayt düzeni | Boy |
|---|---|---|
| challenge isteği / challenge | değişmedi: `[3][u64 nonce][u64 cookie]` | 18 B / 18 B |
| proof | `[3][u64 nonce][u64 cookie][0][u8 caps][msg1]` — caps HEP var (msg1 sabit konumda, bayt 19); msg1 = `e`(32) + boş yükün tag'i (16) | 67 B (msg1 48..80 B → 67..99 B) |
| accept | `[2][u32 1][msg2]` — `ACK{1}` + msg2 = `e`(32) + şifreli `{cid u64, reset jetonu 16}` + tag(16) | 77 B |
| SEALED c→s | `[0x40\|faz][u64 cid][u64 sayaç][şifreli iç datagram][tag 16]` | iç + 33 B |
| SEALED s→c | `[0x40\|faz][u64 sayaç][şifreli iç datagram][tag 16]` | iç + 25 B |

İç datagram düz metin kapının datagramının aynısı (RAW, REL, ACK, FRAG,
PROBE, REPORT, `PATH_CHALLENGE [7][u64]`, `PATH_RESPONSE [8][u64]` —
artık etiketsiz, iç tür). Yazıcının bütçesi iç datagramındır
(1472 − 25); istemci c→s için 1472 − 33 kalır. Noise prologue bağlamı
HELLO nonce'u + çerez (16 B): başka çerez alışverişinden yakalanan msg1
tutmaz. Reset jetonu `token(cid)`: kapının reset anahtarından (B5b:
config'deki ya da statik anahtardan türetilen, kapının adresine bağlı —
aşağıda "Anahtar fazları ve stateless reset"; tel B5a'dakinin aynısı).

*Sıra (kural):* çerez doğrulaması → msg1 biçimi (yalnız boy; düz metin
proof burada `udp_proofs_refused_plaintext`, bozuk boy
`udp_handshakes_malformed`) → kaynak başına sınır (B89) → **küresel DH
bütçesi** (B119, `udp_handshakes_per_sec`, vars. 1000/s, kova 50 ms'lik;
boşken `udp_proofs_refused_budget`) → CID çekimi → DH
(`Msg1::cookie_verified`; doğrulanamayan msg1 `udp_handshakes_failed_decrypt`).
Reddedilen proof hiçbir şey kurmaz, kabul almaz; istemci yeniden yollar.
Mutasyonla kilitli: DH bütçeden önce koşarsa, ya da bütçe kaynak
sınırından önce jeton harcarsa test kırılır.

*İdempotent proof:* accept datagramı oturumla saklanır; oturumun ilk
kaydı açılana kadar aynı adresten doğrulanan proof AYNI baytları alır —
ikinci DH yok (testte tek jetonlu kova + bayt eşitliği; mutasyonla
kilitli). İlk kayıt açılınca saklanan accept bırakılır (istemci oturumu
tutuyor, proof'u bir daha yollamaz; 100k oturumda 77 B × 100k
tutulmaz).

*İki yarı, paylaşım yok:* oturumun `Opener`'ı demux'ta (soketi okuyan tek
görev), `Sealer`'ı yazıcıda. Demux'ın kendi s→c datagramları — güvenilir
bandın birikimli ACK'i, PATH_CHALLENGE — yazıcıya `UDP_SEND` (opcode 16)
isteğiyle gider (piggyback ACK gibi); yazıcı mühürler: tek sayaç alanı,
tek sahip, kilit yok. Güvenilir band iç datagramı tutar; her yeniden
gönderim YENİ sayaçla mühürlenir. Kanal reddederse sayılır
(`udp_acks_not_queued`, `udp_path_challenges_not_queued`); yazıcıda
soket reddederse `udp_acks_send_failed` / `udp_path_challenges_send_failed`.

*Retler:* açılmayan her datagram `seal::Refusal` adıyla sayılır
(`seal_integrity_limit`, `seal_malformed`, `seal_too_old`,
`seal_replayed`, `seal_wrong_phase`, `seal_forged`); mühürlü kapıda düz
metin oturum datagramı `udp_datagrams_unsealed`; bilinmeyen CID
`udp_cid_unknown`. Bütünlük sınırı (2^36 sahte) ve yazıcının sayaç
tavanı (2^62) oturumu kapatır (`stream_rejected`,
`udp_sessions_ended_seal_limit`). İstemci aynı adları
`UdpClientStats`'ta tutar.

*Göç (RUDP-SECURITY §7):* başka adresten gelen kayıt yalnız (1) açıldıysa
ve (2) şimdiye kadarki en yüksek sayaçsa (`Opened::newest`) yol
doğrulaması başlatır; (3) challenge ve yanıt şifreli. Açılan ama en yeni
olmayan kayıt işlenir (istemcinindir), göç başlatmaz
(`udp_path_candidates_not_newest`). Koklanmış CID + sahte kayıt AEAD'de
düşer: doğrulama başlamaz (testli, mutasyonla kilitli). Mühürlü kapıda
`udp_migration` ayarlanmamışsa açık (B112); `false` ise başka adresten
gelen kayıt hiç açılmaz (`udp_datagrams_no_session`), adres değişimi
yeni oturum + resume.

*Uyumluluk matrisi (rUDP hattının tek bilinçli kırılması, §5):*

| İstemci → kapı | Sonuç |
|---|---|
| düz metin → düz metin | bayt bayt B5a öncesi |
| mühürlü (anahtar sabitli) → mühürlü | Noise + SEALED |
| düz metin / eski → mühürlü | proof reddedilir: `udp_proofs_refused_plaintext` (+ kapı başına bir `warn`); istemci `TimedOut` |
| mühürlü → düz metin | düz accept (msg2 yok) reddedilir, sayılır (`accepts_unsealed`); sahte olabilir diye beklemeye devam; süre dolunca `ConnectionRefused` |
| başka anahtar sabitleyen → mühürlü | `udp_handshakes_failed_decrypt`; istemci `TimedOut` |

*Kimlik:* sunucu statik X25519 özel anahtarı config'den: `udp_static_key`
(64 hex) ya da `udp_static_key_file`; mühürlü kapıda yoksa, ikisi
birden varsa ya da bozuksa başlatma hatası (sessiz düz metin yok);
hiçbir log ve hata anahtarın bir karakterini taşımaz. Açık yarı bind'de
loglanır ve `ServerHandle::udp_public_key`'dedir; istemci onu sabitler
(`UdpClientConfig::server_key`, `gsb_client::connect::udp(addr, key)`).
Testler ve yük üreteci çalışma anında kendi anahtarını üretir
(`gsb_server::ephemeral_udp_key`).

*Ölçüm (yük 11–38 — makine meşguldü; ayrıntı RUDP-SECURITY §15):*
responder el sıkışması 130–136 µs; demux'ın gelen datagram başına
işi düz metin ~135 ns, mühürlü ~1,3 µs (açma ~1,17 µs ekler) → 100k
oturum × 10 dg/s'de ~1,2 çekirdek ek: tek demux'ın sınırı (~60–70k
oturum bu hızda; BACKLOG b5a). 1000 istemcilik katılma fırtınası (4 MiB
arabellek): düz metin p99 0,08–0,13 sn; mühürlü bütçe 1000/s'de p99
1,0–1,4 sn (bütçe belirliyor), bütçesiz ya da 5000/s'de ~0,3 sn.

*Reddedilenler:* (1) demux'a ayrı sayaç alanlı ikinci bir `Sealer`
(çift/tek sayaç ya da alt anahtar): istemcinin tek replay penceresi iki
hızla ilerleyen sayaç alanını kaldıramaz (yavaş taraf `TooOld` olur),
alt anahtar ise yönü ikiye böler — `UDP_SEND` bir kanal atlaması
pahasına tek sahibi korur. (2) Demux ACK'ini düz metin bırakmak: düz
metin ACK'e güvenen istemci enjeksiyona açılır; "her datagram mühürlü"
kuralının tek istisnası olurdu. (3) Bütçeyi kaynak sınırından önce
bakmak: sınırın reddettiği proof jeton yakardı. (4) Mühürlü kapıda düz
metin istemciye özel bir ret datagramı: eski istemci onu okuyamaz;
sunucu sayar ve loglar, istemci zaman aşımıyla biter. (5) Saklanan
msg2'yi oturum boyu tutmak (bellek), ya da msg1'i karşılaştırmak: aynı
adres + aynı nonce'a bağlı doğrulanan çerez aynı el sıkışmadır; sahte
msg1'e aynı msg2'yi vermek saldırgana bir şey kazandırmaz (istemcinin
efemeral anahtarı olmadan çözülemez).

**Anahtar fazları ve stateless reset (BACKLOG B5b — 2026-10-03; tasarım
`docs/RUDP-SECURITY.md` §6, §8, §16; bakımcı kararları 7, 9).** rUDP
kripto hattının son turu. Kod: `udp::sealed::{rekey, door}`,
`udp::demux::reset`, `seal::reset` (`reset_datagram`, `reset_tail`,
`ResetKey::{derived_from, for_door}`).

*Rekey politikası:* her mühürlü gönderme yarısı (yazıcının s→c'si,
istemcinin c→s'si; iki yön bağımsız) `SendHalf`'tır: faz **2 dakika ya da
2^20 kayıt** sonra biter, hangisi önce gelirse (`RekeyPolicy::default`;
`UdpTransportConfig::rekey`, `UdpClientConfig::rekey`; sunucu config
anahtarı yok — motorun politikası). Gerekçe: rekey bir sınırın cevabı
değil — ChaCha20-Poly1305'in 2^62'nin altında pratik gizlilik sınırı yok,
sayaç fazlar boyunca sürer (2^62 tavanı oturumundur), 2^36 bütünlük
sınırı tüm anahtarlar üzerinden sahteleri sayar. Rekey ele geçirilen bir
anahtarın açtığını daraltır (REKEY tek yönlü: önceki fazlar kapalı
kalır); süre WireGuard'ın REKEY_AFTER_TIME'ı. Oyun hızlarında (saniyede
onlarca–yüzlerce kayıt) süre önce dolar; kayıt sayısı toplu gönderenin
fazını sınırlar. Çekirdeğin iki kuralı kapı olarak kalır: fazda en az
1024 kayıt (yavaş oturum sonra rekey eder, sayılmaz) ve eşin onayı.

*Onay (ACK → sayaç eşlemesi):* her REL çerçevesinin **ilk**
gönderiminin kayıt sayacı tutulur (yeniden gönderim yeni sayaç alır; ACK
herhangi bir kopyaya cevap olabilir, güvenle kefil olunabilen yalnız
ilki — en küçüğü); birikimli ACK çerçeveyi kapsayınca sayaç
`Sealer::note_peer_ack`'e gider. Sınırlı: `RETRANSIT_CAP`, bandın
kendi çerçeveleriyle bırakılır. Canlı oturumda heartbeat iki yönün REL
bandını da besler. Hiç onaylamayan eş hiçbir şeyi durdurmaz: oturum
mevcut anahtarla mühürlemeye devam eder, her erteleme 10 sn'de en çok
bir kez sayılır (`udp_rekeys_unconfirmed`, istemcide
`rekeys_unconfirmed`), ilk onayda hemen rekey (`udp_rekeys`,
`rekeys`). Faz sınırında sırasız gelen kayıt açıcının önceki anahtar
toleransıyla açılır (testli).

*Stateless reset:* demux, hiçbir oturumun tutmadığı CID'li SEALED
kayda (oturum bitti, ya da sunucu yeniden başladı) o CID'nin reset
datagramıyla cevap verir; istemci oturumu hemen bitirir, 5 sn REL
sınırını beklemez, resume yoluna (yeni el sıkışma + AUTH) geçer.

| Datagram | Bayt düzeni | Boy |
|---|---|---|
| reset s→c | `[0x40 \| rastgele faz][rastgele sayaç < 2^62, u64][rastgele dolgu][jeton 16]` | `min(tetikleyen − 1, 41)`, en az 26 B |

- **Anahtar:** `udp_reset_key` / `udp_reset_key_file` (64 hex,
  opsiyonel); yoksa statik anahtardan türetilir
  (`HMAC-BLAKE2s(statik, "gsb-rudp-reset-key/1")`) — statik anahtar
  mühürlü kapıda zorunlu ve yeniden başlatmadan sağ çıkar (istemciler
  onu sabitler), ikinci sırrı yönetmek gerekmez. İkisi de kapının bağlı
  adresine bağlanır (`for_door`): bir kapı başka bir kapının canlı
  oturumunun jetonunu asla vermez. Yeniden başlayan sunucu aynı adrese
  bağlanmalı.
- **Amplifikasyon yok:** reset tetikleyenden kesin kısa ve en çok 41 B;
  yalnız tetikleyenin kaynağına. İki uç birbirini bilinmeyen sanırsa her
  turda bir bayt kısalır, 33 B'nin (c→s kaydın en kısası) altında durur.
- **Oran:** kapı başına jeton kovası (`udp_stateless_resets_per_sec`,
  vars. 10 000/s, 50 ms'lik; `0` = reset yok), her işten (entropi,
  HMAC) önce; aşan tetikleyici düşer, sayılır
  (`udp_stateless_resets_rate_limited`). Gönderilen
  `udp_stateless_resets_sent`, soket reddi
  `udp_stateless_resets_send_failed`; tetikleyen datagram her durumda
  `udp_cid_unknown`.
- **İstemci:** açılamayan (bütünlük sınırı dışındaki retlerden biri),
  SEALED türlü, reset boyundaki (26..=41 B) datagramın son 16 baytını
  msg2'nin jetonuyla sabit zamanlı karşılaştırır. Tutarsa oturum biter
  ve YALNIZ `stateless_resets_received` sayılır; tutmazsa kendi
  `seal_*` adıyla sayılır, ayrıca `stateless_resets_invalid` ("bunlardan"
  — anahtarı değişmiş bir sunucunun reseti ile küçük sahte kayıt bilerek
  ayırt edilemez).
- **Görünürlük:** reset küçük bir s→c kaydının biçiminde; sayaç düz
  gittiğinden oturumun sayaçlarını izleyen gözlemci rastgele bir sayaç
  görür (sayaç gizleme §10'da, sonra).

*CID rotasyonu (karar 7):* bu turda yapılmadı — ucuz değil (çok CID'li
oturum indeksi, CID başına jeton, emeklilik, göçle etkileşim); tasarımı
RUDP-SECURITY §10'da, BACKLOG'da ayrı satır.

**MTU — oyun bandı parçalanır (rUDP parçalama turu).**
`max_datagram_bytes` varsayılan 1472 (1500−20−8) ve her datagram için
geçerli. Eskiden bütçe üstü çıkan datagram atılır + sayılırdı; oda
tarafındaki `max_snapshot_bytes` uyarısı "grubu böl" sinyaliydi. İki açık
bulgu aynı kökten geliyordu: arenanın takım sisi full'ları ~150 birimin
üstünde 1400 B'yi aşıyor (G3-1), kümelenmiş MMO yükünde
`snap_overflows` ~32 k (CROSS-SHARD §4c). **Karar: çözüm taşımada** —
rUDP'de parçalama + yeniden birleştirme; böylece her oyun ve her kare
türü (grup snapshot'ı, one-shot `Private` full, keep-alive full) tek
yerde kapsanır, çekirdeğin "grup başına tek payload" sözleşmesi ve kitin
zarfı değişmez (`gsb-core`/`gsb-kit` kodu değişmedi).

*Önce ölçüm (afe7fba, geçici enstrümantasyon, loopback):* en büyük tek
kayıt 17 B — her mesaj temiz bölünür. TCP'de (tam nüfus) arena 200 full
p50/p99/max 1748/1905/1923 B, arena 500 4430/5099/5128 B; MMO 500
varsayılan botta aşımların çoğu **delta** (p50 1834 B, 4641 deltanın
4152'si 1472 üstü), keep-alive full p50 1870 B; MMO 500 düello
(`--mmo-duel-frac 0.2`, 90 sn) 32 481 aşımın 31 252'si delta, 1230'u
keep-alive full, 1'i taze full; tepe 2566 B. rUDP'de (bağlantılar
`--stagger-ms 5` ile yayılmış) yazıcının attığı: arena 200'de grup
datagram'larının %80'i, arena 500'de %93'ü; MMO 500'de 2424 grup full +
319 private full, düelloda 9646 + 749. Kayıp full'lar botların kendini
görmesini de engelliyordu (MMO'da nüfus shard 0'da yığıldı).

- **Kapsam:** yalnız **oyun bandı** (op ≥ 1000), sunucu → istemci. Kare
  birimi ne taşıdığına bakmaz: grup delta'sı, grup full'ü, keep-alive
  full'ü, private full — hepsi aynı yoldan.
- **Tel:** `4 FRAG [u16 LE mesaj id][u8 index][u8 count][parça]`.
  Parçalar index sırasıyla birleşince RAW datagram'ının tür baytından
  sonraki baytlar (`[u16 op][payload]`) çıkar. Parçalar eşit (`bütçe −
  5` B), sonuncu kısa. Mesaj id oturum başına, sarmalı, seri sırayla
  karşılaştırılır. **Parçalanmayan her datagram bayt-bayt aynı**
  (bütçe içi RAW, REL, ACK, HELLO — test sabitliyor). FRAG'ı bilmeyen
  eski istemci tür 4'ü yok sayar: onun için kare eskisi gibi kaybolur.
- **Kayıp semantiği:** mesaj ancak bütün parçaları gelince teslim
  edilir; yeniden gönderim yok. Parçası eksik mesaj, slot'unu daha yeni
  bir mesaj alınca ya da ilk parçasından 250 ms sonra (`FRAG_MAX_AGE`)
  düşer ve sayılır; geç kalan parçaları reddedilir, mesaj asla
  diriltilmez. Bant eskisi gibi kendini iyileştirir: sonraki full (en geç
  keep-alive) kaybı kapatır.
- **Sınırlar (istemci, hepsi sabit, datagram başına O(1), map yok):**
  mesaj başına en çok **16 parça** (`FRAG_MAX_COUNT`; varsayılan bütçede
  23 472 B — ölçülen en büyük full'ün, arena 1000'in 10 267 B'sinin
  2,3 katı *(W2'den beri en büyük ölçülen kare savaş 1000'in keep-alive
  full'ü, ~18,5 KB — tavanın ~%80'i; CROSS-SHARD §8b.8)*; aşan kare eski yoldan atılır + sayılır, oturum başına tek
  uyarı); aynı anda en çok **4 yarım mesaj** (`FRAG_SLOTS`; slot = id
  mod 4 — yeni mesaj yalnız kendi slot'undaki eskiyi düşürür; bir tick
  oturum başına en çok iki parçalı mesaj taşır: grup karesi + private
  full); oturum başına **64 KiB** tutulan parça (`FRAG_MEM_CAP`; aşınca
  en eski BAŞKA yarım mesaj düşer). Ayrıntı ve saldırı yüzeyi:
  SECURITY §4.1.
- **Kontrol bandı parçalanmaz.** Sunucu → istemci kontrol kareleri
  (AUTH/JOIN/LEAVE sonuçları, HEARTBEAT_ACK, ERROR) onlarca bayt;
  değişken alanları istemcinin tek bir bütçe-içi datagram'la gönderdiğini
  yansıtır. Bütçeyi aşan kontrol karesi bu yüzden bir hata ve **oturumu
  bitirir** (REL bandının ölüm yolu, `rel_dead`), seq harcanmadan önce
  kontrol edilir. Eskiden seq alıp atılıyordu: karşı tarafın kümülatif
  akışı o delikte takılır, oturum 5 sn sonra yanlış sebeple ("ACK
  ilerlemesi yok") ölürdü.
- **İstemci → sunucu parçaları reddedilir** (demux sayar, hiçbir şey
  iletilmez): girdiler onlarca bayt; sunucuda birleştirme durumu, her
  oturumun büyütebileceği ve tüm oturumların paylaştığı tek görevde
  duran bellek olurdu. Yalnız istemci birleştirir.
- **Sayaçlar:** yazıcı (oturum sonunda log) `frag_messages`,
  `frag_datagrams`, `dropped_oversized` (tavan aşımı); istemci
  (`UdpClientStats`) `frag_reassembled`, `frag_dropped_incomplete`,
  `frag_rejected`; loadgen `RESULT` satırında `frag_reassembled`,
  `frag_dropped`. `max_snapshot_bytes`/`snap_overflows` anlamını
  korur (payload boyu eşiği aşan snapshot sayısı) ama rUDP'de artık bir
  **bant genişliği/parçalanma** sinyalidir, kayıp sinyali değil — kayıp
  sinyali `frag_dropped_incomplete`.

*Sonra (A/B, afe7fba ↔ HEAD, dönüşümlü çiftler, `--write-stall-secs 0
--transport udp --stagger-ms 5`):* yazıcının attığı datagram her
senaryoda 0'a indi (arena 200 ~45 k, arena 500 ~100-124 k, demo 200
~54,8 k, MMO 500 düello ~10,4 k), `frag_dropped=0`; `server_hz` 30,
hata ve sunucu kapanışı 0, `joined = left = N`. Önce botlar full'ları
göremediği için oyun farklı oynanıyordu (arena birimleri kıpırdamıyor,
MMO düellocuları kendini görmüyor, 206 oturum idle-sweep), dolayısıyla
adım süresi taban ile değil aynı yükteki TCP ile karşılaştırılır: arena
500 rUDP 968-1120/1408-1840 µs ↔ TCP 1104-1136/1632-1760 µs; MMO düello
rUDP 264-312/328-456 µs, `out_bps_per_conn` ~40,8 k (C2'nin TCP
tablosu 264-280/352-384, ~41 k). Demo 200 TCP A/B gürültü içinde.

*Bu turda bulunan iki istemci hatası (parçalama onları açığa çıkardı,
ikisi de düzeltildi):* (a) istemcinin canlılık saati yalnız boş kuyruklu
bir RTO turunda tazeleniyordu; oyun bandı hiç susmayan istemci turu
hiç almıyor, oturum sonundaki LEAVE ilk turda "5 sn ACK yok" sayılıp
bant ölüyordu → saat artık kuyruk boştan doluya geçerken başlar;
(b) yeniden gönderim turu yalnız okuma zaman aşımında koşuyordu, meşgul
istemci kayıp LEAVE'i hiç yeniden göndermiyordu (arena 500 `left`
221-303) → tur artık her datagram'dan sonra da koşar (O(1)).

*Açık kalan iki bulgu (bu turun kapsamı dışında):* (1) [H turunda
kapandı — yukarıda "El sıkışma kaybı"] istemci proof'u
GÖNDERİNCE bağlı sayılıyor; aynı anda 200+ el sıkışmada sunucu soketinin
alım kuyruğu loopback'te taşıyor, proof kaybolan istemci 5 sn sonra
ölüyor (afe7fba'da 200 istemciden 65-101'i katılabildi) — ölçümler bu
yüzden `--stagger-ms 5` ile; (2) çekirdekte kapanış yarışı: `stop()`
Shutdown'ı gönderip ticker'ı hemen iptal ediyor; registry odanın DOLU
kontrol kanalına `send().await` ile Shutdown yazmayı beklerken oda artık
tick almıyor ve broadcast kapanmıyor (registry bir `Ticker` tutuyor) →
kilitlenme. Stop anında kontrol kapasitesinden (128) fazla canlı üye
kopunca tetikleniyor (geçici logla doğrulandı: `cap=0` bekleyişi). → S turunda
kapandı (§9.1).

**Elenen alternatifler:** (1) *kit seviyesinde çok parçalı snapshot*
(kit grup karesini kendini tanımlayan parçalara böler, istemci
birleştirir) — kayıp davranışı aynı, ama çekirdek seam'i değişir (grup
başına tek payload → birden çok), zarfa protokol alanı girer, her
istemci değişir ve yalnız kitin kurduğu kareler kapsanır (one-shot
`Private` full ya da oyunun kendi büyük karesi dışarıda kalır);
(2) *udp dokümanının eski tavsiyesi, "snapshot grubunu böl"* — bant
genişliği için hâlâ doğru cevap, ama TEK cevap olarak rUDP'yi arenanın
takım sisi ve MMO'nun kümelenmiş kalabalığı için kullanılamaz kılıyordu;
orada grup oyunun kuralıdır; (3) *IP parçalamasına bırakmak* (datagram'ı
bütün gönder, çekirdek bölsün) — IPv4 parça kaybı datagram'ı yine
kaybettirir, IPv6 router'ları parçalamaz, birçok ara kutu IP parçalarını
düpedüz atar; (4) *güvenilir parçalar* (parça başına ACK + yeniden
gönderim) — bant kendini iyileştirir; yeniden gönderilen eski snapshot
sıradakinden değersizdir; (5) *sunucuda da birleştirme* (istemci → sunucu
parçaları) — yukarıda; ihtiyaç yok, yüzey var.

**Boşta kapatma (FIN yok):** demux'un `BTreeSet<(Instant, SessionKey)>`
(B3'ten beri oturumun iç anahtarıyla, adresle değil)
deadline heap'i (gömlekli geçersiz kılma — girdi yalnız
`son_görülme + idle`'e eşkenken geçerli) + `timeout(min_deadline,
recv_from)`. Sweep, oturuma `ConnIn::ServerClosed` yollar ve oturumu
kaldırır — TCP'nin reader-pump clock'unun (maddeler turu) UDP karşılığı.

**Aktörü ölmüş oturum (BACKLOG B6, `udp::demux::reap`).** Eskiden
aktörü çıkmış bir oturum, o adrese bir sonraki datagram kapalı posta
kutusuna çarpana ya da idle sweep'e kadar demux'ta kalıyordu
(`idle_timeout = None` ise sonsuza dek): yazıcısı, kanalları ve adres
yuvası — aynı adresten gelen yeni el sıkışma kurulu bir adrese ait
sayılıp cevapsız kalıyordu. Şimdi oturumu **yazıcısı** bırakır: yazıcı
zaten en geç her `RETRANSIT_TICK`'te (50 ms) uyanır ve aktörün posta kutusunun bir klonunu
tutar. Posta kutusu kapalıysa **ve** kendi güvenilir bandı hiçbir şey
borçlu değilse (aktörün son bildirimi — ERROR 14/9 — ACK'lendi ya da REL
canlılık sınırı ondan vazgeçti) adresi sınırlı bir süreç-içi kuyruğa
(`Reaper`, crossbeam, `try_send`) koyar ve demux'u, demux'un beklediği
tek şeyle uyandırır: aynı soketten demux'un **kendi adresine** bir
baytlık datagram (belirtilmemiş bind adresi aynı ailenin loopback'ine
çevrilir). Demux her uyanışta (datagram, uyandırma ya da idle deadline)
datagramı işlemeden **önce** kuyruğu boşaltır ve adı geçen oturumu
yalnız gerçekten ölüyse — aktörün posta kutusu ya da yazıcının kanalı
kapalıysa — kaldırır (deadline girdisiyle birlikte). Kuyruk yetki
vermez: bayat bir sinyal (adreste artık yeni bir oturum var) ya da sahte
bir uyandırma yalnız O(1) bir denetime mal olur; uyandırma kaynağından
tanınır, içeriği okunmaz. Sınır: aktör çıktıktan sonra ~bir tick (50 ms);
son bildirim hiç ACK'lenmezse `REL_NO_ACK_FATAL` (5 sn) + bir tick. Oturum
bittikten sonra yazıcı kanal kapanana dek (oda DETACH'ı işleyip
göndericisini bırakana dek) çalışır ama gelen kareleri **tele koymaz**
(`drained`): adres o arada yeni bir oturum taşıyor olabilir, oda da
kapalı bir kanal görüp bunu yavaş istemci düşmesi sanmaz. Yeni await,
zamanlayıcı görevi, kilit yok; datagram başına maliyet boş bir
`try_recv`. Sayaçlar demux çıkış logunda (`reaped`, `reap_wakes`) ve
yazıcının oturum logunda (`drained`). Aktörün `ConnectionId`'si demux'ta
tutulmaz: registry satırını aktörün `ConnClosed`'ı bırakır (değişmedi).

Elenenler: (1) *Demux'un periyodik olarak tüm oturumların posta
kutusuna bakması* — O(N) tarama ve sessiz sunucuda da periyodik uyanma
(idle için aynı gerekçeyle elendi, aşağıda "100k ölçeği"). (2) *Yalnız o
adresten datagram gelince bakmak* (ör. HELLO'da) — yuvayı istenince
boşaltır ama yazıcı ve kanallar idle sweep'e kadar (ya da hiç) yaşar.
(3) *Demux'a ikinci beklenen kaynak* (sokete ek bir uyandırma kanalı) —
çoğullama; tek-await kuralı ve lint. (4) *Yazıcının aktör ölünce
çıkması, demux'un yalnız zayıf gönderici tutması* — ACK yolu oturumla
gider: son bildirimin yeniden gönderimi kesilir (B12'nin bildirimi
rUDP'de "en iyi çaba"ya düşer) ve odanın gönderimleri kapalı kanala
çarpıp yavaş istemci düşmesi sayılır. (5) *Adresi uyandırma
datagramının içinde taşımak* (kuyruksuz) — kaybolan uyandırma bilgiyi de
kaybeder; kuyrukla uyandırma yalnız bir ipucudur, meşgul sokette kaybı
hiçbir şeye mal olmaz.

Testler: `udp::tests::reap` (gerçek soket, idle kapalı, eşten tek
datagram yok) — aktör çıkınca yazıcı ≤ 1 sn'de biter ve aynı adres yeni
el sıkışma kurar; ACK'lenmemiş son bildirim oturumu tutar, ACK bitirir;
biten oturumun geç karesi adresin yeni oturumuna ulaşmaz.
`udp::demux::tests::reap` — sinyal yalnız ölü oturumu (aktörü ya da
yazıcısı gitmiş) siler; uyandırma kaynağından tanınır. İlk üçü eski kodda
düştü; mutasyonlar (boşaltmayı kaldırmak, ölüm denetimini kaldırmak,
yazıcı-gitti kolunu kaldırmak, "band borçsuz" şartını kaldırmak,
uyandırmayı göndermemek, `drained` yerine tele koymak) her biri en az
bir testi düşürür.

**100k ölçeği:** datagram başına maliyet = 1 `recv_from` syscall + 1
hash lookup + 1 `BTreeSet` insert (O(log N) ≈ 17 adım) + 1 `try_send`
≈ 0,5 µs. 100k oturum × 2 datagram/sn ≈ **0,1 core**; oturum başına
10 datagram/sn (`all` görünürlüğünün tavanı, 3M datagram/sn) ≈ **1,5
core** — TCP'nin per-connection writer syscall'leriyle aynı mertebeye.
Asıl 100k duvarı çekirdeğin tek-socket pps'si ve fan-out hacmi
(görünürlük) — TCP ile aynı sınıf. Elenen alternatifler (matematik
ROADMAP'te): SO_REUSEPORT shard'leme (%5'ten az kazanır, tek `accept()`
akışını + ortak ConnectionId alanını kırar), per-due O(N) sweep (join
churn altında heap'in 80×'i), `accept` içine gömülü demux (self-
referential future = unsafe/mio olmadan çıkmaz).

**Taşımanın kendi kayıpları metrikte (B58, sayım turu 3).** Bağlantı
aktörlerinin ALTINDA düşen her şey — rUDP demux'ının dolu oturum
kutusunda düşürdüğü kare, yazıcının parça tavanını aşan ya da bant
ölünce terk ettiği kare, WS okuyucusunun dolu kontrol kuyruğunda düşen
kapanış/pong'u, el sıkışan kapıların ret/zaman aşımı/başarısızlığı —
yalnız görev sonu log satırlarındaydı. Artık aynı yol: taşıma görevi
sayaçlarını kendi durumunda tutar, `gsb_net::metrics::Flusher` ile
DELTA'larını `MetricsEvent::Transport(TransportCounters)` olarak sınırlı
metrik kanalına `try_send` eder — meşgulken en çok 500 ms'de bir
(bağlantı aktörünün kadansı), biterken bir kez daha (demux ve WS
okuyucusu `Drop`'ta: dinleyicinin `close`'u demux'ı abort eder; bitiş
örneği dolu kanalda doğurulan göndericiyle gider). Taban yalnız kanal
örneği aldığında ilerler (B59 kuralı); düşen örnek
`transport.metrics_dropped`'ta ve üst düzey `metrics_dropped`'ta.
Katmanlama: `gsb-net` zaten `gsb-core`'a bağlı (posta kutuları,
`ConnIn`); `MetricsEvent` aynı yönde — tersine bağımlılık yok, toplayıcı
taşımayı bilmez, yalnız deltaları toplar. Kompozisyon kökü her kapının
yapılandırmasına toplayıcının kanalını verir (`UdpTransportConfig`,
`WsTransport`, `TlsTransportConfig`, `QuicTransportConfig`'in `metrics`
alanı; `None` = eskisi gibi yalnız log). **Akış pompaları (B66, sayım
turu 4):** düz TCP dahil her akış kapısının (TCP/TLS/QUIC/WS) okuyucu ve
yazıcı pompası da aynı kanala sayar (`spawn_pumps`'ın `metrics`
parametresi, `TcpTransport::metrics`; `pump/lost.rs`). Yazıcı pompası
başarısız yazma ya da yazma tıkanmasıyla biterse yazdığı batch'in
kalanını (düşen/tıkanan kare dahil) ve çıkış kanalında hâlâ duran her
batch'i sayar: kanalı `close()` edip `try_recv` ile boşaltır (sonraki
gönderim göndericide `frames_out_closed`/`sends_closed` olarak sayılır;
çift sayım yok) → `stream_frames_unwritten` (kare) ve
`stream_batches_unwritten` (kanalda kalan batch). **Bu en büyük kalan
kayıptı:** oda (`shipped_*`) ve bağlantı aktörü (`frames_out`) bu
kareleri kanal aldığı için "gönderildi" saymıştı; sayaç "gönderildi ama
sokete hiç yazılmadı" farkıdır. WS kapısının soket yazıcı görevi bir
kuyruk aşağıda aynı kaybı yaşar: başarısız soket yazması ya da eşin
kapanış el sıkışması (`Shutdown`) kuyruktaki oyun karelerini
`stream_frames_unwritten`'a, kontrol karelerini
`ws_control_frames_unwritten`'a katar; gönderilmiş bir kapanış
çerçevesinin arkasındaki oyun kareleri (RFC 6455 §5.5.1) ayrı:
`ws_frames_dropped_after_close` (kapanıştan sonraki kontrol karesi
kuraldır, sayılmaz). Okuyucu pompası kutuya bekleyerek gönderir; ama
sunucu kararlı son kutuyu kapatınca (B60) elindeki kare reddedilir —
bağlantı başına en çok bir kare —, türüne göre
`stream_{requests,actions,control_frames}_dropped_closed` (istek terimi
RPC defterinin akış-kapısı taşıma terimidir). Pompalar bir kez, sonlarında
kayıp varsa gönderir (tek atımlık `Flusher`, son örnek kuralıyla).
rUDP'de aynı turda: yazıcının soketin reddettiği datagramları banda göre,
bant ölünce hiç göndermediği kareler (`udp_frames_unsent`: batch'in
kalanı + kanalda duranlar), demux'ın reddedilen ACK/soru gönderimleri,
kapalı kutuya çözülen kare (`Closed` kolu, türüne göre — `Full` kolu
gibi; istek RPC defterinin terimi) ve oturumsuz adresten gelen datagram.
**`die`'ın yanlış atfı düzeltildi:** bandın ölüm bildirimi (`RelDead`)
dolu posta kutusunda `try_send` ile düşüyor, aktör çıkış kanalını kapalı
bulup kapanışı `outbound_dead` sayıyordu. rUDP yazıcısı artık akış
pompasının hüküm yolunu paylaşır (`pump::verdict`): doğarken posta
kutusundan bir slot ayırır, bildirim kanal kapanmadan o slota senkron
gider. Slot ayrılamamışsa (kutu doğumda dolu) bildirim kapanıştan sonra
teslim edilir ve `writer_verdicts_deferred` sayar (akış pompası için de).
Elenen: `channel::post` — doğurulan gönderici aktörün kutuya baktığı
andan sonra varabilir, yanlış atıf aynen kalırdı.
**Kapanan kapının ve rUDP accept tarafının kayıpları (B74, sayım turu
5).** El sıkışan kapının (WS/TLS/QUIC) `close`'u uçuştaki her el
sıkışmayı keser ve accept döngüsüne kuyruklanmış bitmiş uç noktaları
atar — ikisi de sayılmıyordu. Artık `handshakes_cut_closed` (kesilen
görev kendi `Err` kolunda sayar) ve `handshakes_unaccepted_closed`
(kuyruğu boşaltan kim olursa — `close` ya da kapanışla yarışan
`hand_over` — her birini slotu bırakılmadan ÖNCE sayar; bunlar
`completed`'da da). Zamanlama: kesilen görevler kapanıştan SONRA koşar,
bu yüzden intake görevinin son örneği kapının oturmasını bekler
(`intake/close.rs`, `settle`): hiçbir slot tutulmayana ve hiçbir el
sıkışma görevi (`live` sayacı, her görevin son işi) koşmayana dek,
`Notify` ile, 1 sn sınırlı — kapanış hepsini zaten o an bitirir; TCP
kapısı portu beklemeden bırakır. Yalnız kapı kapalıysa beklenir (açık
kapıda kapanan QUIC uç noktası kuyruğu accept döngüsüne bırakır).
rUDP'de iki uç: demux'ın `Disconnected` kolu — kanıt doğrulandı ama
accept tarafı gitmiş (dinleyici `close`'suz düşürülmüş): oturum hemen
sökülür, kabul gitmez → `udp_sessions_dropped_accept_gone`
(`udp_sessions_dropped_accept_full`'un eşi). Ve kabulü istemciye ZATEN
gitmiş, uç noktası accept kuyruğunda beklerken dinleyici kapanan ya da
giden oturum → `udp_sessions_unaccepted_closed`: kuyruğun öğesi
(`udp/transport/queued.rs`, `Queued`) düşerken uç noktası hâlâ
içindeyse kendini sayar (tek örnek, dolu kanalı aşan son örnek
kuralıyla) — kuyrukta kalan, `close`'un boşalttığı ve kapısı kapanmış
bekleyen accept'in park etmiş iş parçacığının aldığı, hepsi aynı yerde.
Accept uç noktayı ancak kapı geçirdikten sonra alır
(`Queued::into_endpoint`); demux'ın reddedilen gönderimleri (dolu /
gitmiş) uç noktayı geri alıp kendi sayacına sayar, çift sayım yok.
**WS'nin 1001'i dolu kuyrukta (B80, sayım turu 6).** Sunucunun kapanış
çerçevesi (`WsWriter::poll_close`) soket yazıcısının kuyruğuna
`try_send` ile giriyordu; kuyruk doluyken (son batch yavaş okuyana hâlâ
gidiyor) sayılmadan düşüyor, istemci kapanış görmüyordu. Karar: teslim
— kapanış oyun karesi gibi slot bekler (`ws/writer/going_away.rs`,
`Teardown`; `closing` bayrağı ancak slot tutulunca sahiplenilir ki önce
ya da bu arada kuyruklanan okuyucu kapanışı kazansın), yazıcı pompası
kapanışı zaten yazma-tıkanma penceresi altında bekler. Aynı bayt, yalnız
kaybolmadan; istemci teli değişmedi. Teslim edilemeyen:
`ws_teardown_closes_unsent_closed` (kuyruk kapalı: yazıcı başarısız
yazmayla durmuş) ve `ws_teardown_closes_unsent_stalled` (pencere doldu,
`Drop`'ta sayılır; F66'ya kadar adları `ws_going_away_unsent_*`'tı —
B30'dan beri kapanış 1008/1013 de taşıdığı için ad anlamıyla örtüşsün diye
değişti, sayılan şey aynı). Elenenler: (1) *yalnız saymak* (`try_send` hatasını dolu /
kapalı diye saymak) — kayıp en sık yolda (yavaş okuyan istemci, son
batch) sürerdi ve istemci kapanışsız asılı kalırdı; (2) *doğumda ayrılmış
slot* (`pump::verdict` gibi) — kalıcı bir izin soket yazıcısının erken
çıkıştaki boşaltmasını bekletir, kapalı kuyruğa izinle giren kareyi
saymanın yolu kalmazdı. Yan düzeltme: erken çıkışta boşaltma `recv`
ile (izin kalmayana dek) bekler, kapanış anında izin tutan göndericinin
karesi de sayılır.
**Kayıt defteri için önemli:**
rUDP demux'ı dolu kutuda düşürdüğü güvenilir-bant karesini zaten
ACK'lemiştir — istemci onu yeniden göndermez; RPC isteğiyse hiç
yanıtlanmaz. Kare çözülmüştür (opcode bilinir), yani tür ayrımı burada
da yapılır (`conn::FrameKind`): `udp_requests_dropped_full` RPC
defterinin taşıma terimidir (RPC-CONTROL-PLANE §8.3). Elenenler: (1)
*paylaşımlı atomik sayaçlar + sunucuda yoklayan görev* — demux/yazıcı
sayaçları görev-yereldir, yoklama yeni paylaşımlı durum ve yeni bir
görev isterdi (intake'in atomikleri zaten vardı; onları da intake
görevinin kendisi deltaya çevirir); (2) *toplayıcının dinleyici
nesnelerinden okuması* (`Listener::handshake_stats` gibi) — toplayıcı
aktörlere/taşımaya uzanmaz (dışa açım dikişinin kuralı), demux'ın
sayaçları mesajsız okunamaz; (3) *kayıpları bağlantı aktörüne
`ConnIn` ile bildirmek* — kutusu dolu olan aktöre bildirim de düşer,
demux düzeyindeki kayıpların (kötü cookie, boyut aşımı) bağlantısı
yoktur. Yüzey: OPS §3 "Taşıma kapsamı".

## 7. ECS katmanı

- `bevy_ecs` **standalone** olarak kullanılır (Bevy engine'ı değil). Oda
  actor'ü `World`'ün **tek** borçlusu olduğundan ECS'nin çok-ithal
  (multi-borrow) makinesi zaten güvenli çalışır; bizim ek kısıtımız — tek
  thread, sıralı sistemler — bunu daha da basitleştirir.
- `gsb-ecs::System` trait'i el yapımıdır (`fn run(&mut self, &mut World,
  &SystemCtx)`): bevy'nin `SystemParam`/scheduler mekanizması hot path'te
  gereksiz kit olarak dururdu. `SystemRunner` insertion-order çalıştırır.
- **Değişim algılama (dirty tracking):** bevy 0.19'da event/observer
  API'si yeniden tasarlandığı için hot path'te bevy'nin **event/observer**
  mekanizması bilinçli olarak kullanılmıyor; ayrıca ayrı bir versiyon
  bileşeni de yok — eski `EntityVersion` + `bump()` mekanizması denetim
  turunda kaldırıldı (F1'den beri okuyucusuz kalmıştı: karar wire
  içeriğine taşınınca versiyonun tek okuyucusu giderilmiş, ama yazma
  tarafı ve bu maddenin de içinde olduğu 4 doküman onu hâlâ *kullanımda*
  olan mekanizma gibi anlatıyordu). Değişim sinyali **strateji bazında**:
  - **demo (non-spatial) stratejisi:** sinyal **wire içeriğinin
    kendisi** — oyun mantığı, grup snapshot'ını yeniden üretip
    üretmeyeceğini **kendisi** karar verir ve bu kararın defteri
    **grup başına** tutulmalıdır (§4): demo'da son yayınlanan
    snapshot'ın **wire içeriği** (`entity → (x, y)`, wire'ın tam sayı
    konumlarına kesilmiş) tutulur; içerik değiştiyse (konum **veya**
    üyelik) snapshot yeniden kodlanır — içeriği değiştirmeyen hiçbir
    yazım yayınlatmaz (bant israfı yok).
  - **spatial stratejisi (delta yayını):** sinyal bevy'nin
    **bileşen-bazlı change tick'i** — `Changed<Position>` query
    filtresi. Bu, yukarıda elenen event/observer API'sinden **farklı
    bir bevy arayüzüdür** (query filtresi; hot path'te event/observer
    mekanizması yoktur — orijinal elenme gerekçesi aynen geçerlidir) ve
    delta birimi snapshot versiyonu değil, hücre başına **değişim
    listesi**dir (§8.1 "Delta yayın"). Standalone modda baseline'ı
    **oda elle ilerletir**: her `update()` sonunda `world
    .clear_trackers()` (= `increment_change_tick()`) çağrılır; değişim
    penceresi "önceki güncelleme → bu güncelleme arası yazımlar"dır ve
    `Position`'a yazan **her** yazar (sistem, ingest, doğrudan
    `entity_mut`) otomatik işaretlenir — oda tarafında unutulabilecek
    bir `bump()` disiplini yoktur (yapısal garanti; ROADMAP "AOI
    per-cell memo + dirty cell turu", madde B). Despawn bir bileşen
    yazımı değildir: çıkışlar odanın üyelik defteri + değişim listesi
    üzerinden `on_leave`/`update`'te uygulanır.
  - Eski bağlantı başına `last_sent` haritası ve spawn/remove olayları
    kaldırıldı: üyelik, snapshot'ta var olmaya indirgendi (§4/§8).
- `EntityId = u64` core'da ECS'sizdir; oyun crate'i tel kimliğini kendisi
  seçer. Demo, **oda-yerel, monoton, oda ömrü boyunca yeniden
  kullanılmayan** bir wire kimliği (serial) atar (`DemoRoom.next_wire_id` →
  entity'nin `WireId` componenti → tel); atama iki noktada, tek sayaçta:
  `on_join` (oyuncu entity'si — aynı değer `JOIN_ROOM_RESULT`'a da gider,
  yani iki yol tek uzaydadır) ve broadcast geçişi (`on_join` dışından
  gelen, `Position` taşıyan her entity; aşağıda). Bevy'nin `(index,
  generation)` çifti oda içine kalır; neden tam bevy bits telde taşınmaz
  ve kimlik değişmezi (invariant) nasıl korunur: §8.
- **Minting kapısı kapalı (tip düzeyinde):** `WireId(u64)`'ün **field'ı
  private** ve `Default` derive'ı kaldırıldı — yani `WireId(42)` yazılamaz
  (crate dışında zaten, crate içinde de farklı modüllerden) ve
  `WireId::default()` (0 = "atanmamış") üretilemez. Tek inşaat yolu
  `WireId::new(u64)` (`pub(crate)`, `const`) ve o da pratikte yalnız
  **odanın tek minting noktasından** çağrılır: `DemoRoom::next_serial()`
  (sayaç `next_wire_id`; iki çağrı noktası — `on_join` ve broadcast
  yetim-damgası — bu tek noktanın üzerinden geçer). Okuma tarafı açık
  kalmalıydı (`pub const fn get(self)`): kimlik *okumak* meşrudur,
  *üretmek* değil. Bu, yayınlanabilirlik turundaki "yapısal olarak
  kapatılamaz" hükmünün geri kalanını kapatır: sayacın sahibi zaten oda
  actor'ünün yerel durumuydu; eksik olan yalnızca tipin mint tarafının
  tek noktaya indirgenmesiydi (bkz. ROADMAP "metrik + yük turu").
  *(gsb-kit Faz 1a sonrası: `WireId::new` yok; tipin tek inşa yolu
  kit'in `Minter`'ı — `kit/identity.rs`, KIT-ARCHITECTURE §4.4/§8.1.)*
- **Yayınlanabilir küme (= `Position` taşımak) — yapısal ön koşul:** bir
  entity yayınlanabilmesi için `Position` taşıması yeterli ve bu ön
  koşul **yorumda yaşayan bir disiplin değil, kodda yapısal**dır:
  `on_join` dışından spawn edilen her entity (mermi, NPC, tuzak —
  oyuncuya bağlı olmayan ilk entity eklendiğinde kırılacak olan senaryo)
  broadcast geçişinde aynı monoton sayaçtan **taze bir wire kimliği
  damgalanır** ve onu fark eden snapshot'ta görünür; sessizce görünmez
  olamaz. Bu, kompakt kimlik değişikliğinin (`0bf5b79`) öncesi
  sözleşmeyi korur: yayın kümesi hep "Position taşıyanlar" idi, yeni
  kimlik uzayında da aynı küme kalır. Damga idempotent'tir (damgalanmış
  entity bir daha damgalanmaz) ve kararlı durumda maliyeti sıfırdır
  (yetim sorgusu — `Position` var / `WireId` yok — hiçbir entity
  eşlemez). Regresyon:
  `entity_spawned_outside_on_join_is_broadcast_with_fresh_wire_id`.

## 8. Yayın stratejisi ve ölçekleme (100k hedefi)

v1 stratejisi **grup başına tam, kendi kendine yeten snapshot**:

- Oda, tick başına her grup için snapshot'ı **bir kez** kodlar ve üyeleriyle
  referansla paylaşır (§4). Kodlama O(grubun entity sayısı)/grup/tick;
  teslim O(üye) `Bytes` (Arc) klonu — bağlantı başına kodlama **yok**.
  (Eski modelin O(dirty × bağlantı) taraması ve bağlantı başına `last_sent`
  defteri kaldırıldı: 800 oyuncuda, tek hareket entity ile adım maliyeti
  ~65 ms → ~0.14 ms oldu, bkz. §11.)
- **Üyelik** = snapshot'ta var olmak. Sonradan giren bağlantı ilk yayında
  tüm dünyayı görür (join, CONTROL fazında işlendiği için aynı tick'in
  snapshot'ı yeni üyeyi de içerir). Ayrı spawn/remove event'i yok.
- **Full snapshot kendi kendine yeter:** paket kaybı bir sonraki full'la
  kendiliğinden telafi edilir (spatial'de delta paketi üzerine uygulanır;
  yakınsama garantisi keepalive full'ıdır — §8.1 "Delta yayın"). Sıra/
  güvence ihtiyacı yok — istemci, `sequence`'i (global tick indeksi) ≤ son
  kabul edilen olan snapshot'ı atar (sıralama + tekrar güvenli).
- **Değişiklik yoksa yayın durur** (oyun mantığı `snapshot` → `false`);
  keepalive (varsayılan 1 Hz, `RoomConfig::keepalive_hz`) değişmeyen
  grupların son önbellekli snapshot'ını yeniden gönderir — son paketini
  kaybeden istemci kalıcı bayat kalamaz.
- Bağlantı başına tek batch + `try_send`: yavaş istemci sunucuyu
  yavaşlatmaz; atılan batch'in maliyeti 1 snapshot bayatlık. Batch'in
  tek seferlik içeriği (delta modunda one-shot private full, oyunun
  oturum yükü, input ack'i) varsa çekirdek mantığa bildirir
  (`on_batch_dropped`, §4 "Bağlantı başına teslim") ve kit onu yeniden
  kurar: delta istemcisi keepalive yerine sonraki tick'te — uzun bir
  duraklamadan sonra, ilk batch'i geçtiği tick'in ardından
  (`on_batch_resumed`) — iyileşir (F11, KIT-ARCHITECTURE §10 "F11").
  Batch'in RPC yanıtları çekirdeğindir ve kaybolmaz: sonraki kabul
  edilen batch'le, tam bir kez gider (F14, RPC-CONTROL-PLANE §3.1).
- `max_snapshot_bytes` aşımı uyarı loglanır (rUDP MTU hazırlığı; U
  turundan beri rUDP aşan kareyi parçalar — §6 "MTU").
  Varsayılan 1400 bayt (tipik Ethernet MTU'sunun hemen altı); uyarı grup
  başına **bir kez** çıkar, her tick değil.
- **Wire kimliği (identity) ve ölçülen wire boyutu** (wire kimliği turu
  probe'u; gerçek `RoomActor` + `DemoRoom` + kanallar, ham çıktı commit
  raporunda): `EntityRecord.entity` **oda-yerel wire kimliğidir** — oda,
  kimlik başına sıradaki değeri (1'den başlayıp monoton artan) o
  entity'ye stamp'lar — **iki atama noktası, tek sayaç**: `on_join`
  (oyuncu entity'si; aynı değer `JOIN_ROOM_RESULT`'a da gider) ve
  broadcast geçişi (`on_join` dışından gelen her `Position` taşıyan
  entity — §7, "Yayınlanabilir küme") — ve **oda ömrü boyunca bir
  değeri asla yeniden vermez** (bevy slotu geri dönse bile). Neden: kimlik değişmezi —
  istemcinin dünya görüşü son kabul ettiği **full**'dır (spatial'de delta
  paketi üzerine uygulanır — §8.1); geçmiş ve out-of-band "kimlik yeniden
  eşleme" mesajı yoktur. Değişmezin
  gerektirdiği ayrım — "aynı entity hareket etti" vs "aynı kimliği artık
  başka bir entity taşıyor" — ancak ve ancak **iki farklı entity oda
  ömrü boyunca aynı tel kimliğini paylaşamazsa** snapshot'lardan
  ayırt edilebilir: kimlik hem eski hem yeni snapshot'taysa aynı entity
  (hareket etti), sadece yeni snapshot'taysa yeni entity. Bevy'nin
  `(index, generation)` bu garantiyi veriyordu ama bevy 0.19'da
  `to_bits()`'in alt 32 biti `0xFFFFFFFF - index` olduğundan (ölçüldü;
  eski nottaki "bit tersi" ifadesi yanlış, aslında bit komplementi) varint
  her zaman 5 bayt idi — kaydın yarısından fazlası. Alternatifler:
  (a) yalnız bevy *index* (1-2 bayt) — slot yeniden kullanıldığında (bevy
  0.19'da her 129 free'den sonra ölçüldü) kimlik ÇARPIŞIR ve kayıp
  snapshot'larda istemci yeni entity'yi "eski entity teleport oldu" diye
  okur → değişmez KIRILIR; (b) tam bits başka varint biçiminde — alt
  32 bit ≈ 2^32 olduğundan 5 baytun altına inemez. Serial bu yüzden:
  değişmezi birebir korur, 1 bayt (oda spawn sayacı < 128), 2 bayt
  (< 16384), 3 bayt (< 2M). `EntityRecord` tipik **6 bayt** — 3 tag +
  1 baytlık entity varint + 2 × 1 baytlık zigzag koordinat (±50 arenada
  `|v| ≤ 50` → 1 bayt; koordinat 0 ise proto3 default'ı o alanı hiç
  kodlamaz → 4 baytlık kayıt); snapshot içinde kayıt başına +2 bayt
  tag/uzunluk çerçevesi (tipik 8 B/kayıt), `WorldSnapshot` header'ı
  `sequence` alanıdır (tick < 128 iken 2 bayt). Ölçülen toplamlar (öncesi → sonra):
  1 entity = 14 → **10 B**, 100 = 1196 → **796 B**, 800 = 9544 →
  **7017 B** (marjinal ~11,93 → ~8,77 B/kayıt; 800'de 127 kayıt 1
  baytlık, 673 kayıt 2 baytlık varint — serial'lar 128'den sonra 2
  bayta geçer). 1400 baytlık eşik N=118 → **N=171**'de aşılır (1403 B).
  Eski `sfixed32` + `version` kaydı aynı prost ile 18 B idi — eski
  doküman iddiası "21→16 B" sayıların ikisinde de yanlıştı (aslı
  ~18 → ~12). Regresyon: `wire_identity_survives_ecs_slot_reuse` —
  129 join/leave döngüsüyle bevy slot yeniden kullanmasını ZORLAR
  (test boş olmadığını index eşitliğiyle doğrular) ve geri dönen
  slotun taze wire kimliği taşıdığını, dolayısıyla kayıp snapshot'ları
  olan istemcinin yeni entity'yi "yeni entity" olarak okuduğunu
  kilitlemektedir.

Bu, 100k bağlantı hedefi için **doğru v1**'dir çünkü: (a) doğru ve
basittir, (b) sıralı replay/garanti gerektirmez, (c) kodlama maliyeti
bağlantı sayısından bağımsızdır (gruptan bağımlıdır) — ölçülebilir bir
taban sağlar.

**Belgelenmiş sonraki adımlar (öncelik sırası):**
1. **Delta yayın** (son snapshot'tan fark) — bant genişliği kazancı.
   *Kapatıldı (bu tur): `spatial` stratejisi, hücre = kodlama birimi —
   §8.1 "Delta yayın".*
2. **Oda segmentasyonu** (10k+ tek-oda CPU duvarı; görünürlük stratejileri
   turunun D1'i duvarı in-proc istemci doyuğundan ayırmayı bekliyor) —
   AOI + stratejiler zaten kapandı (§8.1); `Visibility` trait'i gerekmedi.
   *Kapatıldı: `sharded` — §8.2.*
3. **Kompresyon (zstd)** — frame batch'leri üzerine ek bir transport
   seçeneği (uzunluk öneki zaten transport'un malı).
4. **Oda bölme/birleştirme (sharding)** ve cross-region.
   *Kısmen: statik, süreç içi bölme `sharded` — §8.2 (shard'lar tek
   süreçte). Süreçler/makineler arası dağıtım DISTRIBUTED'da tasarım
   (`ShardLink`; bugün yalnız süreç içi link); dinamik bölme/birleştirme
   ve cross-region yapılmadı.*

**AOI (mekansal görünürlük) — v1'de tek oda içinde (bu turda):** Yukarıdaki
adım 2'nin *tek oda* kısmı `gsb_game::aoi`'de kapatıldı; **`gsb-core`'e
dokunulmadı** — §4'ün grup mekanizması AOI'nin gerektirdiğinin tamamını
zaten sağlıyor (`group_of` her tick yeniden değerlendirilir → hücre geçişi
otomatik; grup başına snapshot bir kez kodlanır, üyeyle `Arc` ref'iyle
paylaşılır; "değişiklik yoksa yayın durur" defter eşitliği üyelik+konumu
içerir). AOI yalnızca mekansal bir `GroupKey` (`Cell`) sunuyor; soyutlama
sızması yok.

- **Görünürlük kümesi:** oyuncunun hücresinin merkezli **3×3 blok**
  (`RADIUS=1`). Oyuncu başına yarımçap bilerek **yapılmadı**: grup başına
  paylaşılan baytı kırardı (her oyuncunun farklı kümesi → `GroupKey`'i
  `ConnectionId` yapmak, grup başına kodlama gerektirir → bant kazancı sıfır).
- **Hücre:** `floor(pos / cell_size)`; `cell_size` **konfigüredir**
  (`Config.aoi_cell_size`, vars. 20.0) — sabit değil, çünkü doğru boyut
  yoğunluk/arena/MTU'ya göre değişir. `max_snapshot_bytes` (vars. 1400) ile
  ilişkisi: ~12 B/kayıt → 1400 B ≈ 116 kayıt/blok; blok 9 hücre → **~13
  kayıt/hücre** hedef; N büyüdükçe (yoğunluk) hücre küçülmeli.
- **Kimlik değişmezi korunur:** wire id `on_join`/yeni `Position` damgasında
  **bir kez** basılır, hücre değişiminde değişmez; sonradan giren kendi
  hücresinin ilk bloğunda **tam kümesini** görür (test:
  `gsb-kit`'in `aoi::tests`'i + `gsb-demo`'nun `tests/aoi.rs`'i).
- **Ölçülen ticaret:** AOI ~9× kodlama maliyeti taşır (her kayıt 9 komşu
  bloğa girer) ama bant genişliğini O(entity) → O(görünürlük) yapar; hücre
  boyutu küçükçe bant kazancı %80+ (1000/2000). Break-even + yeni darboğaz
  (adım süresi/CPU) CHANGELOG "Kapatılanlar (metrik düzeltme + AOI turu)"
  bölümünde ölçülmüş. Break-even bu turun D1'inde ölçüldü (aşağıda);
  10k+ ölçekli oda segmentasyonu hâlâ P2.

### 8.1 Görünürlük stratejileri (tak-çıkar; görünürlük stratejileri turu)

Yayın stratejisi artık **strategi seçimi**: aynı oyun (aynı component'ler,
aynı hareket sistemi, aynı wire format) üzerinde dört değiştirilebilir
strateji, hepsi §4'ün `RoomLogic` seam'i üzerinde, `Config.visibility`
ile seçilir (sunucu + `gsb-loadgen`):

| strateji | `GroupKey` | görünürlük kuralı | örnek |
|----------|-----------|-------------------|-------|
| `all` (varsayılan) | `()` (1 grup) | her entity her yerde | taban; eski davranış |
| `spatial` | `Cell` (mekansal hücre) | 3×3 hücre bloğu (bu turdan itibaren delta kodlu — §8.1 "Delta yayın") | MMO/AOI (§8, önceki tur) |
| `team` | `Team` (takım başına 1 grup; demo 2 takım atar) | takım üyesi + menzildeki düşman (takım üyelik world state: `TeamMember` componenti; `group_of` world okur) | MOBA/takım sisli |
| `pvs` | `Sector` (harita bölgesi) | statik görünürlük tablosu (elle convex sektörler) | FPS/PVS |

Takım üyeliği **dünya içi bir oyun durumudur** (`TeamMember(Team)` componenti;
join'de atanır — kural `conn.id` paritesi, `team_of` sadece join anı kuralıdır):
`group_of`, AOI'nin `Position`'u okuması gibi world state okur. Runtime takım
değişimi (component yazımı) bir sonraki tick'te group yeniden değerlendirmesiyle
takip edilir, **wire id değişmez** (test kilitli: `runtime_team_change_moves_the_
group_and_keeps_the_wire_identity`). Dört stratejinin tümü artık aynı şekildedir:
"grup = f(world, conn)" — konumdan mı oyun durumundan mı geldiği core mekanik
için fark etmez (ROADMAP "ayrı proses + yayılma profili turu" D maddesi).

**Varsayılan `all`** — üç gerekçeyle: geri uyumluluk (tüm önceki ölçüm
tabanı `all`), ölçüm tabanı (en kötü durum; her stratejinin kazancı
karşısında net okunur), en az sürpriz (görünürlük kısıtlama oyun kararıdır;
sessiz kısıtlama bug olarak algılanır).

**Neden `Visibility` trait'i yok:** stratejiler arasındaki gerçek fark,
`RoomLogic`'in zaten ayırdığı iki şey — `GroupKey` tipi + `group_of`/
`snapshot` içeriği. Trait bu farkı yeniden soyutlar, arkasında duracak
ortak uygulama yoktur (içerik hesapları: küme birleşimi / mesafe önelemi /
statik tablo). Kanıt: 2 yeni strateji + 1 metrik metodu eklendi, core'un
trait şekli değişmedi; tek core değişimi ekleyici bir sayacıdır
(`RoomLogic::encoded_records()` default + `snap_records`, D3 ölçümü için).
Ortak **oda muhasebesi** (identity minting, connection tablosu, input
ingestion, sistem yürütme, orphan stamp) ise 4 odada birebir aynıydı —
`gsb_game::common` modülüne tek kopya olarak taşındı; `common::next_serial`
artık `WireId::new`'ün tek çağrıcısı (gsb-kit Faz 1a sonrası: tek inşa
yolu kit'in `Minter`'ı). Detay + elenen alternatifler: ROADMAP
"Kapatılanlar (görünürlük stratejileri turu)" C maddesi.

**Ölçülenler (N=1 000, 30 Hz; detay ROADMAP D2 ve ayrı-proces turu):**
`spatial c5` bant **%82** az (247,8→44,8 kbps/conn), `pvs` **%49** az
(126,0 kbps/conn), `team` kümeli yük geometrisinde **sıfır** kazanç + 2×
kodlama (25 birim menzil + merkezî kümelenme → her iki takım haritanın
tümünü görüyor; yük profili özelliği, strateji zayıflığı değil). Kodlama
overlap'i ölçüldü (`overlap_x` = kodlanan kayıt/üye/tick): all 1,00 · PVS
1,37 · team 2,00 · c20 5,50 · c5 8,28. "Birim başına tek kodlama, istemci
abone" tasarımı **eşik altında bırakıldı** (spatial için eşik ≈10, ölçülen
8,28; bedel istemci başına 9 frame — D1'de doyan eksenin tahtası).
**Break-even (spatial, hücre 5) ayrı prosesle ölçüldü** (istemciler P
process'te, sunucu pin'li ayrı çekirdek kümesinde; in-proc sayılar
istemci decode'ı sunucuyla paylaştığından duvarı maskelemişti): p50 adım
süresi 33,3 ms bütçeyi **9k-10k arasında** aşıyor (5k 12,5 ms · 8k 25 ms ·
9k 25 ms · 10k ≥50 ms, %54,8 adım bütçeli üstte, `server_hz` 23,2); 10k'da
sunucu çekirdek havuzu **%25** dolu — doyan parça tek room actor'ünün
serisel adım yolu (bir sonraki kaldıraç oda segmentasyonu, P2). **Takım
sisli, geniş harita (spread profili, ±1000):** aynı kod düşman takımın
**~%85'ini** her an paketten dışarıda tutuyor (500 kendi + ~77 düşman;
rec/tick 2 000→1 153); kümeli geometride kazanç sıfır kalıyor — strateji,
geometriye koşullu.

**Delta yayın (spatial strateji; bu tur):** `spatial` stratejisi artık
 snapshot'larını **delta kodlar** — kodlama birimi grup değil **hücredir**:
 her hücrenin parçası tick başına **bir kez** hesaplanıp dondurulmuş
 `Bytes` olarak önbelleğe alınır ve hücre sınıflandırması (negatif —
 `Silent` — sonuçlar dahil) tick başına hücre başına bir kez memo'lanır;
 grup bir **kitle**dir ve grubun paketi, gördüğü hücrelerin parçalarının
 birleşimidir (referansla paylaşım — §14.1'in omurgası aynen). Delta'nın
 kendisi tick başına içerik farkı **değildir**: bir hücrenin deltası, o
 tick'teki **değişim listesi**dir — `Position` yazan her yazar bevy'nin
 change tick'i ile otomatik işaretlenir (§7 "Değişim algılama"), üye
 çıkışları odanın üyelik defterinden gelir; `update` bu listeleri hücre
 başına biriktirir (işaret *yapısal*dır — unutma disiplini yoktur; ROADMAP
 "AOI per-cell memo + dirty cell turu", madde B).
 full/delta kararı **(grup, hücre) başınadır**: yeni doğan grup bir kez full
 alır; devam eden grup delta taşır. Delta değişmezi: bir hücrenin deltası
 `içerik(şimdi) vs içerik(önceki tick)` — gruptan bağımsız, saf hücre
 fonksiyonu; bu ancak her yerleşik grubun istemcilerinin her zaman *önceki
 tick'in* hücre içeriğiyle senkron olması durumunda doğrudur (indüksiyon:
 yayınlayan tick gruptaki her hücreyi güncel içeriğe taşır, sessiz tick'te
 grup sessizdir *çünkü* içerikler eşit). Paket içi sıra sabittir ve wire'da
 görülebilir: `[sequence, delta][removed: entity çıkışları][cell_exits:
 hücre çıkışları][entities: upsert'ler]` — entity iki görünen hücre arasında
 geçerken kaynağından önceki **hücre** çıkışı raporlar (hücre-yerel ve ucuz);
 boşalan hücre **tek** `CellExit` kaydıdır (50 entity'li hücre = 1 kayıt).
 `delta` bayrağı paketin modunu wire'da ayırt edilir kılar: yanlış modu
 bekleyen istemci sessizce yanlış uygulamak yerine davranır. **Geç giriş ve
 grup geçişi:** baseline'ı olmayan yeni grup üyesi (join/crossing) bir kez
 **private full** alır (`Private{ack|snapshot}` oneof'u, op 1004) — grup
 akışının ayrı bir akımıdır, istemci tarafından **koşulsuz** uygulanır
 (grubun gap'de bırakıp gittiği delta ile aynı batch'te, ondan hemen sonra
 gelir); grup o tick'ten sonra delta'da kalır (test kilitli:
 `late_join_sees_full_world_one_shot`, `late_joiner_receives_full_world_
 snapshot` hâlâ canlı). **Keepalive kararı:** her keep tick'inde (vars. 30
 tick / 1 s) her grup — aktif olsun ya da sessiz — **taze** full gönderir
 (aktif grupta o tick'in delta'sının *yerini* alır). Gerekçe: delta modunda
 son paketi yeniden göndermek anlamsızdır (delta, uygulanmadıysa bayat;
 uygulandıysa bilgi yok); taze full, istemci ne kadar delta kaçırmış olursa
 olsun bir keepalive periyodu içinde doğru duruma **yakınsama garantisi**dır
 — ayrı bir resync istemi yok. **İstemci gap kuralı (wire kuantizasyonu
 bulgusu):** akış **olay-odaktır** — grup yalnızca içeriği değişince frame
 gönderir ve wire konumları i32 (kesik) iken hareket 30 Hz × 10 u/sn = tick
 başına 1/3 wire birimi, yani hareket eden entity bile her ~3 tick'te bir
 değişir; grup stream'i dolayısıyla **sıra-düzgün değildir** (seq = oda
 tick'i, frame yalnızca değişimde). O yüzden: seq boşluğu **kayıp kanıtı
 değil, normal durumdur**; istemci baseline'ı olan delta'yı boşluğa rağmen
 **üzerine uygular** (kayıtlar mutlak: konum upsert'i, mutlak wire id ile
 unutma, mutlak hücre ile unutma — idempotent, bayat view üzerinde güvenli;
 en kötü hal = 1 keepalive periyoduna kadar bayatlık; bir kaçırılan çıkış
 geçici hayalet olabilir, taze full tam duruma döndürür); yalnızca
 baseline'ı **olmayan** delta atılır (birinci full'a kadar; one-shot
 private full aynı batch'te iyileştirir). Elenmiş alternatifler: (a)
 "boşlukta atla, tam gelene kadar" (ilk uygulama — kuantizasyon yüzünden
 delta yolunu dejeneratif kılıyordu: aktif grupta delta'ların ~2/3'ü gap
 düşüyordu, istemci her sessizlikten sonra 1 Hz full'ı bekliyordu);
 (b) seq = grup yayın sayacı (ortak saat ölçümünü — loadgen `tick_hz_med`'ın
 (son_seq−ilk_seq)/Δt tanımını — bozuyordu: seq = global tick indeksi kuralı
 korundu). **Sis güvenlik parametresi:** mahalle **içerikten bağımsız** 3×3'tür
 (mahalleye giren hücre, *içerik taşımıyor olsa bile* görünür — içerik
 bazlı mahalle "kim görünüyor"u içerikle karıştırır ve gizlilik kararını
 dünya durumuna bağlardı); tek güvenlik düğmesi `cell_size`'tır: hücre
 büyüdükçe mahalledeki potansiyel entity sayısı artar — `cell_size` küçük =
 sıkı sis. **Oyuncu-bazlı aydınlatılmış hücre saklaması** (aynı hücrede
 bile yalnızca ışık konisi içindekileri gösterme) bu turun kapsamı dışındaydı;
 **A9'da (2026-09-28) kit'e isteğe bağlı yapı taşı olarak geldi:**
 `LitAoiRoom<G: LitGame, S>` (`AoiRoom::with_game(g, s).lit()`). Kural oyunundur —
 `LitGame::light(world, izleyici) -> Option<Light>` (tick başına izleyici
 başına bir kez; `None` = filtre yok) ve `LitGame::lit(&light, world,
 kayıt_entity, &wire) -> bool` (mahalledeki her kayıt için). Işığı olan
 izleyici **kendi grubudur** (`LitGroup::Viewer(p)`): kareleri yalnız
 aydınlık alt kümeden kurulur (takım odasının küme defteri — full / delta
 `removed` + upsert / keep-alive full; `cell_exits` yok, yeni wire alanı
 yok), paylaşılan hücre paketini hiç almaz; aydınlık olmayan kaydın tek
 baytı ona gitmez. Işığı olmayan herkes hücrenin paylaşılan paketini
 aynen alır (ışık yakmayan oyunun baytı `AoiRoom`'unkiyle bayt bayt aynı).
 Maliyet yalnız ışık yakan oyunda: tick başına `P` `light` + `F·V` `lit`
 çağrısı, `F` izleyici için ayrı kodlama, `O(F·V)` durum
 (KIT-ARCHITECTURE §10 "A9").
 **Ölçülen (N=500, 30 sn, spatial, in-proc debug; `still` profili — bkz.
 yük yöntemi):** kayıtların çoğunun hareketsiz olduğu dünyada delta,
 tam-snapshot'ın ~1/67'si kadar kodlama üretir ve kazanç **hareketsizlik
 oranıyla artar** (0.90 → 67×, 0.95 → 77×; oran 1.0'da teorik sınır =
 keepalive full'ları):

 | still_frac | kodlama kayıt/tick (eski→yeni) | bant/conn (eski→yeni) | adım p50 (eski→yeni) |
 |---|---|---|---|
 | 0.90 | 2 884 → **43** (67×) | 44 131 → **7 218** B/sn (6.1×) | 3 126 → 6 250 µs |
 | 0.95 | 2 791 → **36** (77×) | 42 794 → **6 046** B/sn (7.1×) | 3 126 → 6 250 µs |

 Her iki versiyonda `over_budget` %0 (bütçe 33 333 µs). O turun dürüst
 takası (bucket rotasyonu + tick başına hücre fark taraması, adım p50 ~2×)
 bu turda kapatıldı: sınıflandırma memo'landı (negatif sonuçlar dahil) ve
 delta, tick başına içerik farkı yerine **değişim listesi**ne indirgendi
 (madde A + B; ROADMAP "AOI per-cell memo + dirty cell turu" — orada ham
 sayılar + faz dökümü + spek sapmaları). Ring/spread gibi **her entity her
 tick hareket eden** profillerde delta'nın bant kazancı yine yok (orijinal
 ölçüm tabanı aynen — `ring`/`spread` profilleri değişmedi). Ham RESULT
 satırları + makine bilgisi: CHANGELOG "Kapatılanlar (delta yayın + input
 sıralama turu)" C maddesi. Detay + elenen alternatifler: aynı ROADMAP
 bölümü + yeni turun ilgili maddeleri.

 **Yük yöntemi (ayrı-proces turu):** `gsb-loadgen` üç modda: in-proc
(varsayılan, tüm önceki ölçüm tabanı), `--serve` (sunucu process'i;
`--metrics-listen` metrik raporlarını kanal verisinin ikili TCP akışıyla
taşır — rapor stdout'tan okunmaz; stdout'a yalnız bağlanan adresleri
bildiren tek `SERVING` satırı düşer, F31) ve `--orchestrate` (sunucu + P istemci
process'i, tek RESULT). `--pin` (taskset) SMT-farkında ayrık çekirdek
kümeleri verir; RESULT'ta `server_cpu_s`/`clients_cpu_s`/`affinity`
izolasyonu kanıt olarak taşır. `--profile spread` (uniform, geniş harita;
varsayılan `ring` = eski kümeli profil, aynen) stratejileri ayrıştırmak
için eklenmiş ikinci yük geometrisidir.

### 8.2 Oda segmentasyonu (`sharded`) — topoloji düzeyi strateji

Görünürlük stratejileri (§8.1) *tek odanın içinde* snapshot'ları gruba
bölerek ölçekler; dünya hâlâ tek actor'da, tek `World`'de, tek tick
gövesindedir. `sharded` bunun **üstüne**, *topoloji* düzeyinde bir
katmandır: **tek oda N shard actor'üne** bölünür. N shard'ın her biri
haritanın bir kesitini (grid hücresini) **tek başına** sahip olan bağımsız
bir `World` + `ShardLogic`'tir (`gsb-kit`'in `ShardedRoom`'u, `gsb-demo`'da örneklenir); paralellik
hedefe ulaşıp var olan oda actor modelinin kendisinden gelir (N görev),
thread havuzu değil. Bu, §14.4'te "katman eklenmeden sığmıyor" olarak
notlanan *sınır olmayan tek dünya* sınıfının ilk uygulamasıdır.

**Topoloji.** `shard_count` (1..=16; `grid_shape` en kareye yakın
rows×cols'u seçer) shard haritayı grid'e böler. Shard'lar yalnız
**4-komşu** (batı/doğu/kuzey/güney) ile konuşur; komşu olmayan çiftler
arasında kanal **yoktur**. Her shard, komşularına giden birer `mpsc`
kanalı tutar (matris; boş yuvalar dummy). Shard actor'ü kendi
`World`'ünün **tek sahibi**dir — başka hiçbir görev o `World`'e dokunmaz.

**Shard tick'i (6 faz, tek senkron gövde; §4'ün oda tick'ine benzer):**
`CONTROL` (önce `deferred` kuyruğu, sonra kanal `try_recv`) → `READ` →
`CONVERT` → `SYSTEMS` → `MIGRATE` → `BORDER` → `BROADCAST`. Fazlar §4'teki
oda fazlarıyla aynı disiplindedir: gövde senkrondur, await yok; shard
arası veri yalnız kanaldan akar.

**Migrasyon (sınır geçişi).** Bir entity, hareketle komşu shard'ın
hücresi içine girdiğinde (AOI ile aynı bölge testi) shard onu
**migre eder**. Protokol iki değişmezi korur:
1. **Hiçbir tick dizininde entity iki shard'da da yoktur, hiçbirisinde de
   yoktur** (tek sahip). Gönderen, `Migrate` mesajı *başarıyla*
   `try_send`'edildiyse entity'yi `t+1`'de despawn eder (MIGRATE 4a);
   başarısızsa (`Full|Closed`) mesaj geri alınır, bağlantı `conns`'a
   geri sarılır, entity **orada kalır** (bir sonraki tick yeniden dener).
2. **Kurulum kapısı (install gate):** alıcı, `Migrate{at_tick}`'i
   `ctx.tick <= at_tick` iken *erken* ulaşırsa `deferred`'e erteler —
   entity tam olarak `at_tick+1`'in `CONTROL`'unda spawn olur. Böylece
   "gönderen despawn etmeden alıcı spawn eder" yarışı kapanır.
   `Leave`/`Migrate` yarışı iki tablo epoch şemasıyla çözülür
   (`conn_epoch` = kurulu join'ın epoch'u; `conn_tombstone` = işlenen en
   yüksek LEAVE epoch'u; kapı **tombstone**'a bakar, *kurulu* epoch'a
   değil — ping-pong migrasyonu aynı join'ın epoch'unu taşır, kurulu
   tabloya baksaydı canlı join'ı reddederdi). Detay + testler: ROADMAP
   "Kapatılanlar (oda segmentasyonu turu)".

**Wire kimliği (iç içe basım — A30; önceden range partitioning).**
`N` shard'lı odada shard `i`'nin `n`'inci çekimi `(n − 1) · N + i + 1`
(`gsb_core::shard::interleaved_id`): her shard mod `N`'de kendi kalıntı
sınıfını basar. Bu, (a) id'lerin shard'lar arası **çakışmasız** olmasını
sağlar (ayrık sınıflar, sayaçlar ne olursa olsun), (b) id'in
**migrasyonda değişmemesini** sağlar (entity taşınırken `WireId`'siyle
gider — istemci dünyasında kimlik stabil; alıcıda çekim değil), (c) bir
enkarnasyonda id'in **yeniden kullanılmamasını** sağlar (sayaç geri
gitmez), (d) id'leri **küçük** tutar: her shard `n` çektiğinde oda tam
`1 ..= n·N`'yi kullanmıştır (eski `i * 2^20` aralıkları 3–4 baytlık
varint'ti, kaydın %19–40'ı). Shard başına çekim sınırı
`SHARD_SERIAL_CAPACITY = 1 << 20`: join bu sınırı aşacaksa `RoomFull`
(sessiz taşma yok); her id ≤ `2^20 · N` (eski tavan). Paylaşımlı (global)
sayacı reddetme nedeni: join senkron tick gövdesinde koştuğu için global
bir "sonraki id" kaynağı shard'lar arası senkronizasyon gerektirirdi; iç
içe basım (range partitioning gibi) bunu sıfır senkronizasyonla çözer.
Tasarım, değişmezler ve elenen alternatifler: KIT-ARCHITECTURE §10 "A30".

**Sınır görünürlüğü (1-tick hizalama).** Komşu shard'lar her tick
`BORDER` fazında sınıra yakın entity'lerinin **tam durumunu**
(`BorderExchange`) değiştirir. Shard, komşunun sınır entity'lerini
*kendi* snapshot'una **borrow** olarak ekler (özellikle: kendi entity'si
bir tick geriden borçlu kopyayla çakışırsa **kendi kaydı kazanır** — core
`own_wires` filtresiyle). Border marjı **çeyrek hücre**
(`min(cell_w, cell_h)/4`): sınırın iki tarafını da kapatır ama borçlu
kümesini/hizlamayı sınırlı tutar (tam hücre marjı dejeneredir — s×s
hücredeki her nokta bir kenardan ≤s uzakta olur, bütün shard export
olurdu). Tüketici tarafındaki **frame filtresi** (`in_border_frame`)
komşunun *tüm* sınır export'ından yalnızca kendi görünüm çerçevesine
düşen kayıtları alır. Sonuç: sınırda iki oyuncu birbirini **görür**
(test: `boundary_entities_are_visible_to_both_sides`); bedel = sınır
entity'si en fazla 1 tick geriden görünür (lag-blink: entity ≤1 tick
yok, asla kopyalanmaz).

**Bağlantı sahipliği.** Entity migrasyonla shard'lar arası taşınırken
**bağlantı kanalları da entity'yle gider** (`Migrate` mesajı `out`/`in`
mailbox'larını taşır) — yeni shard, entity'nin snapshot'larını doğrudan
o bağlantının writer'ına yollar. Registry shard'ları takip etmez
(bağlantının *şu anki* shard'ını bilmez); bu, shard'lar arası
kayb-ekleme (lost-update) durumunu önler. `Leave` ise **tüm** shard'lara
yayılır (epoch 0 ile; `max` birleşmesinde zararsız) — düşük frekanslı bir
olayda N−1 no-op kabul edilebilir, kayıp güncelleme değil.

**Kapasite.** Cap oda düzeyinde **tama** tutulur (registry
`members + pending >= cap`); shard başına `ceil(cap/N)` değil —
shard'ın join'ı (kimliği kendi sayacından basar), cap'in shard'lar arası
dağılımını bilmez.

**Ölçümle sonuç (ayrı proses, `all`, 10k, 30 Hz; detay ROADMAP'te):**
tek oda (`all`) 10k'da adım bütçesini aşıyor (p50 üstü adımlar,
`server_hz` ~30 ama `step_max` ~73 ms); `sharded N=4` aynı 10k'da adım
bütçesine **içe** giriyor (`over_budget` ~0), `sharded N=8` p50 adımı
~8× düşürüyor. Fiyat: shard protokolü (kanal trafik + border değişimi +
migrasyon + N× actor) sunucu CPU'yu ~1,4-1,5× artırıyor. Yani
**segmentasyon adım-duvarını (tick bütçesini) kaydırır; fan-out/decode
duvarını kaydırmaz** — onu kaydıran mesafe-yanlı görünürlüktür (§8.1).
Segmentasyon, *tek sürekli dünya* sınıfına aittir; bölünebilir oyunlar
için çoklu oda yeterlidir (her oda bütçe içine sığar).

## 9. Kapanma (shutdown) kaskadı

Abort'siz, kanal kapanmalarına dayalı:

```text
ServerHandle::stop
  → önce kapılar (F41): HTTP ops kapısı ve her listener close() — bekleyen
    accept "listener kapandı" hatasıyla biter → accept loop, elindeki son
    bağlantıyı (ConnOpened + aktör) teslim edip kendiliğinden döner (B16);
    stop() döngüleri tek bir süre sınırı (ACCEPT_STOP_GRACE, 1 sn) altında
    bekler, aşanı abort eder (geri sigorta; ağaç içi taşımalarda hiç
    gerekmez; abort aktör doğmadan önceki bir await'te keser)
  → RegistryMsg::Shutdown   (döngülerin gönderdiği her ConnOpened onun önünde)
      → kutu KAPANIR, arkasında kalanlar boşaltılıp türe göre sayılır,
        registry'nin son örneği post edilir (F53, aşağıda)
      → her dispatcher'a RoomOp::Close (yol açma + son detach), sonra senders düşer
        (Close'u kuyruğa giremeyen dispatcher kuyruğu kapanınca aynısını yapar, B61)
      → her bağlantının inbox'ına ConnIn::Shutdown  (spawn'lu gönderim)
      → her odaya kontrol kanalından RoomControl::Shutdown — BEKLEMEden
        (try_send; kanal doluysa spawn'lu gönderici, §9.1); bir sonraki
        tick'te işlenir
  → ticker.abort()   (görev göndericisi düşer; registry'ninki de düşünce
                     broadcast kapanır = geri sigorta: kontrol Shutdown'ını
                     görememiş her oda, recv'de Closed görüp temiz çıkar)
  → connection actor'ler son çerçeve olarak ERROR 14'ü try_send eder
    (beklemesiz; kuyruk doluysa düşer — §5.6) → çıkar → in_tx/out_tx düşer
      → reader pump: send hatası → çıkar
      → writer pump: kanal kapanır → kuyruktakini (bildirim dahil) yazar
        → socket close (write-stall penceresi altında; QUIC'te ACK beklenir)
  → registry: Shutdown işlenince run() break eder (kendi mailbox klonunu tuttuğu
    için EOF'ı bekleyemezdi — artık beklemez); düşerken Ticker klonunu da
    düşürür — broadcast'i kapatan son halka (§9.1)
  → stop(): odaların düşme bariyeri — registry ve her oda/shard görevi
    bitti (on_shutdown + match_result koştu) — tek bir süre sınırı
    (SERVICE_STOP_GRACE, 1 sn) altında beklenir (§9.2)
  → kayıtlı her oyun servisine senkron durdurma isteği (ör. ekonomide
    kuyruğun arkasına Stop); servisler ikinci bir 1 sn'lik tek son tarih
    altında join edilir, aşan abort edilir ve StopReport'ta sayılır
  → metrik toplayıcı: ticker'ın broadcast'i kapanınca Closed görür,
    artık metrik kanalını bekler: her oturum üreticisi (registry, oda/
    shard, dağıtıcı, bağlantı aktörü, accept hattı) göndericisini düşürene
    dek — odaların RoomFinal'ı, bağlantıların son flush'ı — katlar, sonra
    son raporu basıp temiz çıkar (F35; sınır FINAL_REPORT_GRACE = 2 sn,
    kapanıştan itibaren, yukarıdaki beklemelerle yan yana koşar;
    StopReport::final_report_complete, bkz. §12)
```

`Listener::close` kapısı rUDP turunda kullanıldı: `stop()` listener'ı
kapatır (F41'den beri registry Shutdown'ından ÖNCE, aşağıda) — TCP'de
kapısını kapatır, rUDP'de demux
görevini sonlandırır (socket klonu + endpoint göndericisi düşer;
writer'lar aktör kaskadıyla çıkar), QUIC'te yeni bağlantıları reddeder
(`set_server_config(None)`) ama canlıları KESMEZ — `Endpoint::close`
akıştaki durdurma bildirimini terk ediyordu (§5.6). Kural: `close` canlı
oturumları kısa kesmez; onlar aktör kaskadıyla, bildirimleriyle biter.

**Accept döngüsünün kibar sonu (BACKLOG B16).** Eskiden `stop()`
listener'ları kapatıp accept görevlerini `JoinHandle::abort` ile
kesiyordu — kaskadın son sert abort'u. Abort döngüyü herhangi bir
await'inde keser: `accept` ile aktörün spawn'ı arasındaki
`registry.send(ConnOpened).await`'te kesilen döngü pump'ları başlamış,
aktörü doğmamış bir bağlantı bırakabilirdi. Şimdi her ağaç içi
listener bir **kapı** (`gsb_net::transport::Door`,
`CancellationToken::run_until_cancelled`) taşır ve `accept`'inin
tamamını — soket accept'ini ve TLS/WS/QUIC el sıkışmasını — onun
içinden çalıştırır. *(B31'den beri el sıkışma `accept`'in dışında,
bağlantı başına görevde — §6 "El sıkışan kapılar"; kabul görevi, her
el sıkışma görevi ve kuyruğu bekleyen `accept` aynı kapının altında,
`close()` hepsini keser. Sözleşme aynı; `StopReport` ağaç içi
kapılarda yine 0 abort.)* `close()` kapıyı kapatır: bekleyen accept ve
sonrakilerin hepsi `listener_closed()` hatasıyla biter
(`is_listener_closed` işaretçisinden tanır, türünden değil); accept
loop bu hatada döner. Döngünün tek await'i yine `accept()`'tir; kapı o
tek future'ın parçasıdır — pump deyimindeki `timeout(d, read)`'in
deadline'ı gibi yalnız BİTİREBİLİR, döngüye ikinci bir iş akışı
vermez. Kestiği şey henüz oturum olmamış bağlantıdır (soket accept'i ya
da uçuştaki el sıkışma) — kapalı bir kapının zaten reddettiği şey;
canlı oturumlar kapıyı görmez (B12 kuralı: aktör kaskadı). rUDP'de
kapı ayrıca bloklayan havuzdaki crossbeam `recv`'ini beklemeden accept'i
bitirir (demux abort'u endpoint göndericisini düşürünce o iplik de
çıkar); QUIC'te `set_server_config(None)`'a ek olarak kapanır, uç nokta
kapanmışsa (`accept` → `None`) da aynı hata döner. `stop()` döngüleri
tek bir son tarih altında bekler (`ACCEPT_STOP_GRACE` = 1 sn, döngü
başına değil) ve yalnız aşanı abort eder — kapısı olmayan üçüncü taraf
bir `Listener` için geri sigorta; `stop()` her koşulda biter (S kuralı
korunur: bekleme tek bir join'in süre sınırlı beklenişidir). Ne olduğu
`StopReport`'ta: `accept_loops_ended` / `accept_loops_aborted` (ağaç
içi taşımalarda 0); abort olursa `warn`.

**HTTP ops yüzeyi de aynı kapıyla durur (B33).** Ops yüzeyinin accept
döngüsü (`http.rs`) B16'dan sonra `stop()`'un abort ettiği son accept
döngüsüydü. Artık accept'i aynı `Door`'dan geçer (`door.admit(listener
.accept())`; tek await yine accept); `stop()` kapısını dinleyicilerle
aynı noktada kapatır, döngü `listener_closed` hatasında döner ve
listener'ı düşürür. Görevi oyun dinleyicilerinin döngüleriyle AYNI son
tarih altında beklenir, aşarsa aynı geri sigortayla abort edilir.
`StopReport`'a yeni alan eklenmedi: ops döngüsü de bir accept döngüsü
olarak `accept_loops_ended`/`accept_loops_aborted`'a sayılır
(`http_listen` açıkken dinleyici sayısı + 1): aynı mekanizma, aynı
son tarih, aynı sayaç — ve açık yapının literal kullanıcıları kırılmaz;
abort sayısının sıfırdan büyük olması hangi döngü olursa olsun aynı
şeyi söyler (kapısı accept'ini bitirmeyen bir döngü). Önceden
kabul edilmiş ops bağlantıları kendi kısa ömürlü görevlerinde kalır
(tek yanıt + sınırlı boşaltma), kapı onlara dokunmaz. Test:
`accept_stop.rs::the_ops_http_accept_loop_ends_on_its_closed_door`
(TCP dinleyicisi + ops yüzeyi, sessiz bir scraper bağlıyken: 2 döngü
bitti, 0 abort, < 0,9 sn, `stop()` sonrası ops portu bağlantı
reddeder); mutasyonlar: kapalı-hata kolunu kaldırmak ya da kapıyı
kapatmamak testi düşürür (döngü 1 sn sonra abort edilir).

Elenenler: (1) *Döngüde select ile kapanış jetonu beklemek* — lint ve
tek-await kuralı; döngü iki kaynağı çoğullamaya başlar. (2) *Kendine
bağlanıp uyandırmak* (kapanışta listener'ın adresine bir TCP bağlantısı
açmak) — belirtilmemiş adresin loopback'e eşlenmesine, güvenlik
duvarına ve backlog'a bağlı; QUIC/UDP'de karşılığı yok. (3) *Dinleyen
sokete `shutdown(2)`* — Linux'ta accept'i uyandırır, macOS'ta
`ENOTCONN` döner; ayrıca `socket2` doğrudan bağımlılığı ister ve
uçuştaki el sıkışmayı kesmez. (4) *İç kabul görevi + kanal* (listener
kendi görevinde accept edip kanala koyar, `close` o görevi abort eder)
— abort'u kaldırmaz, listener'ın içine taşır; accept başına bir kanal
atlaması ekler. *(B31 bu şekli aldı, ama abort'suz: görevler kapının
altında biter; kanal atlaması artık eşzamanlı el sıkışmanın bedeli —
B16'nın sahip olmadığı bir gerekçe.)* (5) *Yalnız soket accept'ini kapıdan geçirmek* — uçuştaki
TLS/WS el sıkışması `stop()`'u kendi süre sınırına (saniyeler) kadar
tutardı.

Testler: `gsb-server/tests/accept_stop.rs` — beş kapılı sunucuda
`stop()` beş döngünün beşini de "kendiliğinden bitti" sayar, 0 abort, <
0,9 sn; TLS ve WS kapısında sessiz eşin tuttuğu el sıkışma varken de
aynı. `gsb-net` `transport::door::tests` (+ `tls::tests`,
`quic::tests`) — her taşımanın `close`'u park etmiş accept'i bitirir,
sonraki accept de kapalı döner; TLS/WS'te el sıkışmada bekleyen accept
de. `boot::stop::tests` — geri sigorta: süreyi aşan döngüler tek son
tarihte abort edilir. Mutasyonlar: döngünün kapalı-hata kolunu
kaldırmak, TCP/WS/QUIC kapısını kapatmamak ya da TLS/WS accept'ini
kapıdan geçirmemek sunucu testlerinin ikisini de düşürür (TCP'ninki
`gsb-net` testini de). rUDP'de kapı kaldırılırsa döngü yine biter (demux
abort'u alıcıyı kapatır, o yol da `listener_closed` döner) — iki yol.

**Önce kapılar (BACKLOG F41).** Yukarıdaki sıra eskiden tersti: `stop()`
registry'ye `Shutdown`'ı gönderip kapıları ANCAK ondan sonra kapatıyordu
(yorumu "önce kapılar" dese de). Registry `Shutdown`'da okumayı bırakır:
arkasına kuyruklanan `ConnOpened` posta kutusuyla birlikte düşer,
sonrakinin gönderimi hata alır — accept döngüsü (`let _ =`) aktörü yine
doğurur. O aktör `ConnIn::Shutdown` almaz (registry onu hiç tanımadı),
ERROR 14 göndermez; eşi soketi kapatana ya da idle penceresine (30 sn)
dek yaşar, duruşu aşar ve metrik göndericisini tuttuğu için son rapor
`FINAL_REPORT_GRACE`'te onun son sözü olmadan çıkar
(`final_report_complete = false`). Pencere: bir döngünün accept'i ile
`ConnOpened` gönderimi arası (çok iş parçacığında gerçek paralellik; dolu
bir registry posta kutusunda gönderimler sırayla beklediğinden
genişler) ve kapının kapandığı yoklamada biten accept (`Door::admit`
elindeki bağlantıya meyillidir — kapanışla aynı anda hazır olan bağlantı
yine döner).

Düzeltme `boot/stop.rs`'te yalnız sıra: kapılar kapanır, `stop()`
döngülerin BİTMESİNİ bekler (`end_accepts`, 1 sn tek son tarih), registry
`Shutdown`'ı ANCAK ondan sonra alır, ticker hemen ardından kesilir.
Döngü yalnız accept'in "kapandı" hatasında döner; o ana dek aldığı her
bağlantıyı (`ConnOpened` + aktör) teslim etmiştir, dolayısıyla her
`ConnOpened` registry'nin kutusunda `Shutdown`'ın önündedir ve her aktör
kayıtlı, bildirimli (ERROR 14) biter. Süre aşımında abort edilen döngü
aktör doğurmaz: iki await'i (accept, `ConnOpened` gönderimi) ikisi de
spawn'dan önce; abort'ta endpoint düşer, pump'lar kanalları kapanınca
çıkar. Kapıdan sonra gelen eş hiç kabul edilmez; el sıkışan kapıda
kesilen/kuyrukta kalan el sıkışma, rUDP'de kuyruktaki oturum zaten
sayılıyor (`handshakes_cut_closed`, `handshakes_unaccepted_closed`,
`udp_sessions_unaccepted_closed`, B74) — yeni sayaç gerekmedi.

Sınırlar ve korunanlar: `stop()`'un üst sınırı aynı (accept 1 sn, sonra
odalar 1 sn + servisler 1 sn ile toplayıcının 2 sn'lik sınırı yan yana:
en çok ≈ 3 sn — eskiden de öyleydi; toplayıcının sınırı artık accept
beklemesinden SONRA başlar, çünkü ticker ondan sonra kesilir). S kuralı
değişmedi: registry hiçbir odayı beklemez; `stop()`'un döngü beklemesi
canlı registry'ye karşı tek bir süre sınırlı join'dir (döngünün
`ConnOpened` gönderimini okuyan registry henüz `Shutdown` almamıştır).
Accept beklemesi boyunca odalar tick'lemeye devam eder; kapısı kapanan
rUDP'nin demux'ı durur — eskiden de `Shutdown` işlenmeden duruyordu.
İstemci teli değişmedi.

Elenenler: (1) *Reddedilen `ConnOpened`'da aktörü doğurmamak* (gönderim
hatasında soketi ERROR 14'le kapatmak) — tek başına yetmez: `Shutdown`'ın
arkasına kuyruklanıp registry'yle düşen `ConnOpened`'ın gönderimi
BAŞARILI döner, hata görünmez; düzeltmeden sonra `stop()` yolunda bu kol
hiç koşmaz (yalnız ölü registry'de — o zaman sunucu zaten ayakta değil).
(2) *Kapıları `Shutdown`'dan önce kapatıp döngüleri ondan sonra beklemek* —
kapanışla aynı yoklamada biten accept'in `ConnOpened`'ı yine `Shutdown`'ın
arkasına düşer (aşağıdaki üçüncü test bu mutasyonu yakalar). (3)
*Registry'nin `Shutdown`'dan sonra kutusunu boşaltmaya devam edip geç
gelenlere `ConnIn::Shutdown` göndermesi* — kutunun EOF'u hiç gelmez
(registry kendi klonunu tutar, §9.1); ne kadar boşaltacağı bir süre
sınırına kalır, geç gelen yine kaçabilir. Sıra yapıdan verilebiliyor.

Testler (`boot::stop::tests::window`, paused saat, gerçek accept döngüsü,
bağlantı aktörü, toplayıcı ve `stop()`; test kapısı + kutusu dolu, test
"başla" diyene dek okumayan sahte registry — gönderimler sırayla bekler,
pencere böylece deterministik): duruş sürerken gelen eş `Shutdown`'ın
arkasına teslim edilmez, kapıda reddedilir, rapor tam; duruştan önce
alınıp `ConnOpened`'ı dolu kutuda bekleyen eş `Shutdown`'ın önünde
kaydedilir ve biter; kapının kapandığı yoklamada biten accept'in eşi de
`Shutdown`'ın önünde kaydedilir. Eski sırayla ilk ve üçüncü düşer
(`[Closed(BUSY), Shutdown, Opened(c1)]`, `final_report_complete=false`);
"kapat, `Shutdown`, sonra bekle" mutasyonunda üçüncü düşer.

**Duruşta takım export'u kapalı kutuya (F50).** F41'in sırası registry'nin
odalardan ÖNCE çıkmasını değiştirmedi (S kuralı: registry hiçbir odayı
beklemez). Registry `Shutdown`'ı işleyince her shard'a `ShardMsg::Shutdown`
bırakır ve döner; posta kutusunun alıcısı onunla düşer. `Shutdown`'ını
kutusunu boşalttıktan SONRA alan shard o adımını bitirir: takım fazı
(adımın sonunda) export'unu kapalı kutuya gönderir, `try_send` `Closed`
döner — `team_export_drops_closed` (dolu kutu `team_export_drops_full`,
registry çalışırken gerçek kayıp). Sonraki adımın boşaltması `Shutdown`'ı
bulur ve shard durur: kutusuna yerinde teslim edilen `Shutdown`'la shard
başına en çok bir kapalı ret. Garanti değil: shard'ın kutusu doluysa
`Shutdown` spawn'lu göndericiden (`post`) sonra gelir, shard aradaki
tamponlu tick'lerde yine adımlayıp yine reddedilebilir — bu yüzden
kapalı sayısına test sınır koymaz. Eşi — `Shutdown` kutuda sırasını
beklerken arkasına BAŞARIYLA kuyruklanan export — F53'e dek registry'yle
birlikte sayılmadan düşüyordu; artık `team_exports_unread` (aşağıda).

**Registry'nin kutusunda kalanlar (F53).** Registry `Shutdown`'dan sonra
hiçbir şey okumaz ve kendi posta kutusunun bir klonunu tuttuğu için
EOF'u beklemez (§9.1). Önceden alıcıyı düşürüyordu: `Shutdown` sırasını
beklerken arkasına kuyruklanan her mesaj — göndericisi gönderimi
BAŞARILI görmüştü — onunla birlikte sayılmadan gidiyordu. Artık
`Shutdown` kolu (`registry/actor/run/leftovers.rs`) önce kutuyu KAPATIR
— sonraki gönderim göndericide reddedilir ve gönderici kapalı reddi
sayıyorsa orada sayılır (shard'ın export'u `team_export_drops_closed`) —
sonra `try_recv` ile boşaltır (kapalı kutu sonludur; F41'in elediği
"boşaltmaya devam" seçeneğinin sonu gelmeyen EOF sorunu yok) ve kalanları
duruş anındaki tablolara göre, türüne göre ele alır. B68'in oda/shard
için yaptığının registry eşi; ölçüt: kalan, taşıdığı şey HİÇBİR yerde
olmayacaksa ve başka hiçbir sayaç onu görmüyorsa sayılır.

- **Takım export'u** (odanın canlı enkarnasyonu, sharded kayıt): shard onu
  `team_exports`'ta "kuyruklandı" saydı, hub hiç rölelemedi →
  `team_exports_unread`. Bayat enkarnasyonun ya da bilinmeyen odanın
  export'u hub çalışırken de sessiz no-op'tur; burada da sayılmaz.
- **Katılma** (`SpawnPlayer`, resume denemeleri dahil): hiç işlenmedi;
  yanıtı düşer, bağlantı istemcisine `ERROR` "registry unavailable" der →
  `joins_unread`. Katılma hattının diğer her durağı kaybını sayıyor
  (B57 `join_ops_dropped`, B75 `joins_refused_closed`, B68
  `joins_unprocessed`); bu durak eksikti.
- **Duruşun arkasında açılan bağlantı** (`ConnOpened`): kayıtlılar gibi
  `ConnIn::Shutdown` alır (spawn'lu gönderim) — kayıp yok, sayaç yok.
  Sunucunun `stop()`'u kapıları önce kapattığından (F41) bu kol yalnız
  registry'yi kendisi süren kütüphane kullanıcısında koşar.
- **Sayılmayanlar** — duruşun kendisi onları yapar: kopuş (`ConnClosed`)
  ve ayrılış (`DespawnPlayer`): teardown'un `RoomOp::Close`'u her
  dağıtıcıya tuttuğu üyeliği detach ettirir ve her oda durur; dağıtıcı
  yankıları (`SpawnDone`, `SpawnFailed`, `LeaveDone`, `DetachDone`,
  `OpsClosed`), `RoomDied` (panik bekçide sayılır, B67) ve `Authed`
  yalnız teardown'un düşürdüğü tabloları günceller; kontrol düzlemi
  istekleri (`CreateRoom`,
  `DestroyRoom`, `RoomStatus`): düşen yanıt çağırana hatadır. Eşleşme
  tümdür (joker kol yok): yeni bir mesaj türü burada karar ister.
- **Odanın hükümleri** (`CloseConn`, `LeaveConn`, `DetachDespawned`):
  F53 onları da "teardown yapar" diye saymıyordu (B57'nin gerekçesi);
  F56'dan beri kayıp hüküm olarak sayılıyor (aşağıda "Duruşun yuttuğu
  hükümler").

Sayılar registry'nin SON örneğinde gider: `Shutdown` kolu sayımdan sonra,
teardown'dan (`on_shutdown`) önce örneği `channel::post` ile gönderir —
dolu metrik kanalında spawn'lu gönderici kendi klonunu tutar — ve
göndericisini ancak `run()`'dan dönerken düşürür. Toplayıcının son raporu
her oturum üreticisinin göndericisini düşürmesini beklediğinden (F35), o
örnek son rapordan önce katlanır. Tablo göstergeleri (`rooms`, `conns`)
son örnekte teardown'dan önceki değerlerdir. S kuralı değişmedi: sayım
senkron, registry hiçbir odayı beklemez. İstemci teli değişmedi.

Elenenler: (1) *Kalanları işlemek* (ör. katılmayı dağıtıcıya vermek) —
duruşta odalar durur; işlenen katılma hemen teardown'a düşer, iş
sayımdan fazlasını getirmez ve registry'nin duruş yolunu uzatır. (2)
*Kutuyu `on_shutdown`'dan SONRA kapatmak* (BACKLOG adayı) — teardown
sırasında gelenleri de boşaltırdı ama tablolar o an silinmiş olur (canlı
enkarnasyon sınaması yapılamaz) ve çok iş parçacığında teardown'un
kendi yankıları (dağıtıcıların `DetachDone`'u) kutuya karışırdı; önce
kapatmak onları göndericide reddeder — sayılacak bir şey taşımıyorlar. (3) *Tek bir
"okunmamış mesaj" sayacı* — karışık anlam; türlerin çoğu kayıp
taşımıyor. (4) *Son örneği `try_send` ile göndermek* — dolu kanalda
düşer; sayım kaybolurdu.

**Duruşun yuttuğu hükümler (F56; B57'nin kararı yeniden).** Oda/shard
üyeliği kendi başına bitirince registry'ye hüküm verir: kapatma
(`CloseConn` — oyunun atması E8, girdi-boşta tavanının `disconnect`'i
E6), ayrılma (`LeaveConn` — tavanın varsayılan `leave_room`'u, B40),
detach-despawn raporu (`DetachDespawned`). B57 duruşta kapalı registry
kutusunun reddettiği hükümleri "çağlayan zaten her şeyi söker" diye
saymadı, F53 kutuda okunmayanları aynı gerekçeyle saymadı. Oysa hüküm
kaybolur: istemci hükmün `ERROR 9`'u ve gerekçesi yerine duruşun
`ERROR 14`'ünü alır, `server_closes{idle_input|kicked}` hükmü hiç
yazmaz, satır yerleşmez. Artık sayılıyor. Bir hüküm duruşta şu
yerlerden TAM BİRİNDE yakalanır; her yer kendi yakaladığını sayar:

1. **Odanın kuyruğunda** — registry kutusu doluydu, istek sonraki
   tick'i beklerken oda durdu: odanın/shard'ın `finish`'i
   (`RoomCounters::count_unsent_verdicts`).
2. **Registry'nin kapalı kutusu reddetti** — `Shutdown` kolu kutuyu
   kapattı (F53): odanın flush'ı (`flush_close_requests`,
   `flush_leave_requests`, `flush_despawn_reports`; `Closed` kolu artık
   düşürür VE sayar).
3. **Registry kutusunda `Shutdown`'ın arkasında** — boşaltma
   (`registry/actor/run/leftovers.rs`).
4. **Bağlantının kutusunda duruşun `ConnIn::Shutdown`'ının arkasında** —
   registry hükmü işledi ama bağlantı önce duruşu okudu: bağlantı
   aktörünün sonu (`abandon_inbox`). Bu ayak her sunucu hükmü için
   geçerli (pompanın `idle_timeout`'u, yok edilen odanın `RoomGone`'u da)
   ve oturum başına yalnız İLKİ sayılır — oturum tek gerekçe yazar;
   bir hükmün ya da istemcinin kendi sonunun arkasındaki hüküm hiçbir
   şey kaybettirmez (oturum zaten bitti).
5. **Duruşun bildirimi spawn'lu yedeği geçti** — bağlantının kutusu
   doluyken işlenen hüküm yedek göndericide bekliyordu, duruşun
   bildirimi bağlantıya önce vardı: bildirim o hükmü ADIYLA taşır
   (`ConnIn::ShutdownOvertaking`), onu ilk okuyan bağlantının sonu sayar
   (`abandon_inbox`; F58'in ret noktasındaki sayımının yerine F60 —
   aşağıda "Oturum başına tek kayıp hüküm (F60)").

Her yer saydığını tek `MetricsEvent::VerdictsLost` ile gönderir
(durdurma-mesajı deyimi; oda/shard son örneğinden ÖNCE, hiçbir şey
kaybolmadıysa hiç), toplayıcı registry diliminde toplar
(`RegistryReport::verdicts_lost`, `metrics::VerdictsLost`): kapatmalar
gerekçeye göre (`close_verdicts_lost{reason}`, `server_closes` ile aynı
etiket kümesi — ikisinin toplamı kararı verilen oturum sonu), ayrılmalar
ve detach-despawn raporları ayrı. Registry boşaltması tabloya bakmaz:
etkisi boş olacak hüküm (bağlantısı zaten gitmiş) de sayılır, çünkü
odanın kapalı-kutu reddi tabloya bakamaz — sayaç HÜKMÜ sayar, etkiyi
değil; iki yer aynı ölçütle sayar. Registry dilimi, çünkü hükmün
taşıyıcısı registry ve sebep onun duruşu (F53/F54 ile yan yana).

**Registry'nin bağlantıya hükmü yerinde (F57).** 4. ayağın bir ucu
sayılamıyordu: registry işlediği hükmü bağlantıya spawn'lu gönderimle
yolluyordu, duruşun `ConnIn::Shutdown`'ı da spawn'lu gider. Hüküm
arkada varıp bağlantı kutusunu kapattıktan SONRA gelirse reddediliyor
ve sayılmıyordu (gönderici oturumun duruşla mı istemciyle mi bittiğini
bilemez). Artık registry'nin bağlantıya her bildirimi (odanın kapatma
hükmü, `LeftRoom`, `superseded`, doğum tavanları, `RoomGone`)
`channel::post` ile gider: kutuda yer varsa YERİNDE — registry'nin
ondan sonra gönderdiği her şeyin, duruşun `Shutdown`'ının da önünde
(registry hükmü `Shutdown` kolundan önce işler) —, yalnız kutu doluyken
spawn'lu göndericiden. Kilit: `registry::actor::close::tests` (hüküm
registry onu işlediği an bağlantının kutusunda, duruşun bildirimi
arkasında; tavan reddi ve `RoomGone` da yerinde; eski spawn'lu
gönderimle ikisi de kırmızı).

**Spawn'lu yedeğin reddi sayılıyor (F58).** Bağlantının kutusu doluyken
işlenen hüküm spawn'lu yedekle gider ve aynı yarışa girebilir:
duruşun bildirimi önce varır, bağlantı kutusunu kapatır, yedek
reddedilir — hiçbir yer saymıyordu. F58 onu reddin olduğu yerde saydı
(bu paragraf o hâli anlatır; F60 sayımı bağlantıya taşıdı, `post_or`
kalktı — aşağıda):
registry'nin bağlantıya her HÜKMÜ (`tell_closed`'un kapatma hükmü,
`superseded`, doğum tavanları, iki `RoomGone`) `Registry::tell` ile
gider (`registry/actor/tell.rs`), o da `channel::post_or` ile —
`post`'un reddi geri bildiren eşi (mesaj yerinde reddedilirse hemen,
spawn'lu gönderici reddedilirse orada geri verilir). Red anında
registry durmuşsa (kendi kutusu kapalı: `Shutdown` kolu kutuyu kimseye
haber vermeden ÖNCE kapatır) hüküm gerekçesiyle tek
`MetricsEvent::VerdictsLost` olarak sayılır (`close_verdicts_lost`);
registry çalışırken red, kendi kendine bitmiş bir bağlantıdır — kutusundaki
istemci sonunun arkasındaki hüküm gibi, kayıp yok. Hüküm taşımayan
bildirim (`LeftRoom`: registry satırı zaten yerleştirdi) `post`'ta kaldı;
eşleme bağlantının sonuyla ortak (`ConnIn::verdict`). (F60 bu
ret-noktası sayımını kaldırdı: iki oturumda yanlış sayıyordu — aşağıda.)
Kilit (F58 hâli): `registry::actor::close::tests::refused`
(dolu kutu, hüküm spawn'lu yedekte, registry durur, bağlantı biter →
1 `kicked`; aynı yoldan yok edilen odanın `RoomGone`'u → 1 `room_gone`;
aynısı registry çalışırken → 0; `ConnIn::verdict` eşlemesi her kolda;
önce kırmızı: eski `post` ile iki red testi 0 gördü; mutasyonlar —
`is_closed` denetimi yok, yedeğin reddi bildirilmiyor, `tell` yerine
`post`, eşleme kolları — hepsi kırıldı).

Elenenler: (1) *Yer başına ayrı aile* (`…_unread`, `…_refused`,
`…_unsent`) — aynı kayıp, F55'in şikâyet ettiği zamanlamaya bağlı
bölünme; tek aile her hükmü bir kez sayar. (2) *Yalnız
`idle_input`/`kicked` için iki sayaç* — `CloseRequest::cause` genel bir
`ServerClose`; 4. ayak her gerekçeyi görür. (3) *Oda sayımını odanın
son örneğine (`StopCounts`) koymak* — kayıp registry'nin; registry
boşaltmasının ve bağlantının saydığıyla tek ailede toplanamazdı.
Kilit: `registry::close::tests`, `room::tests::idle::stop`,
`shard::tests::idle::stop`, `registry::actor::run::leftovers::tests`,
`conn_counts::stopped`.

**Oturum başına tek kayıp hüküm (F60).** F58'in ret noktasındaki sayımı
oturumun nasıl bittiğini bilmeden sayıyordu; iki yanlışı vardı, ikisi de
dolu bağlantı kutusu + duruş penceresi ister: (a) bağlantının sonu
duruşun arkasında bir hüküm (pompanınki) bulup saydı, registry'nin aynı
oturuma spawn'lu yedekteki hükmü de duruştan sonra reddedilip sayıldı —
bir oturum için iki kayıp; (b) istemcisi (ya da başka bir hüküm) oturumu
önce bitirmiş bağlantının reddettiği yedek de, registry durmuşsa,
sayılıyordu — oysa oturum zaten bitmişti, kayıp yok (bağlantının kendi
sonu da istemci sonunun arkasındaki hükmü saymaz). Oturumun nasıl
bittiğini yalnız bağlantı bilir; sayım ona taşındı:

- registry yedeğe düşen (yerinde kuyruklanamayan) İLK hükmü satırına
  yazar (`ConnInfo::verdict_in_flight`, `registry/actor/tell.rs`;
  `channel::post_where` yerinde/yedek/ret ayrımını söyler). Satırsız
  bağlantı (doğumda tavan reddi) duruş bildirimi almaz — hükmü okur ya
  da kendi biter, kayıp yok;
- duruşun bildirimi o satır için `ConnIn::Shutdown` yerine
  `ConnIn::ShutdownOvertaking(hüküm)`'dür. Bağlantı onu ilk okursa duruş
  gibi biter (ERROR 14, aynı tel) ve adı geçen hükmü TEK kayıp hüküm
  olarak sayar — hüküm arkasında da olsa, kapanışta reddedilse de;
  arkasında bulunan başka bir hüküm (pompanınki) ikinci kez sayılmaz.
  Etiket: adı geçen hüküm, çünkü registry onu duruşundan ÖNCE verdi;
  arkadaki hüküm duruştan sonra da verilmiş olabilir;
- hükmü önce okuyan bağlantı onunla biter (`server_closes`'ta, kayıp
  değil) ve duruş bildirimini hiç okumaz; kendi biten bağlantı ikisini de
  okumaz. Spawn'lu yedeğin reddi bu yüzden hiçbir yerde sayılmaz: oturum
  hangi yolla bittiyse sonu kaybını saymıştır.

Satır alanı hiç temizlenmez: hüküm oturumu bitirir; satırı duruşa kadar
yaşayan bağlantı hükmü henüz okumamıştır (okuduysa `ConnClosed` satırı
söker ya da `inbox`'ı düşürür). `LeftRoom` hüküm değildir, bildirimi
düz `Shutdown` kalır. Elenen: bağlantı ile yedek arasında paylaşılan bir
"oturum sonu" atomiği (kanal dışı ortak durum; duruş bildirimi zaten
bağlantıya giden tek sıralı yol), registry'nin duruşu yedeğin arkasına
zincirlemesi (bağlantı başına sıralı röle görevi — aynı sonucu daha
pahalı verir). Kilit: `registry::actor::close::tests::twice` (önce
kırmızı: (a) 2 saydı, (b) iki biçimde 1 saydı) ve `…::refused` (duruş
yedekteki hükmü — ilkini — adıyla taşır; yerinde giden hüküm ve
`LeftRoom` için düz `Shutdown`; ret sayılmaz).

**Kapalı registry'ye katılma (F54).** `Shutdown` kolu kutuyu kapattıktan
sonra hâlâ yaşayan bir bağlantıya gelen JOIN'in `SpawnPlayer` gönderimi
bağlantı aktöründe reddedilir (`conn/actor/room.rs`); istemci `ERROR`
"registry gone" alır. Katılma hiç kuyruklanmadı — registry'nin arkasında
onu görecek kimse yok. Artık reddi gören tek yer sayar: bağlantı aktörü
`MetricsEvent::JoinUnsent` gönderir (durdurma-mesajı deyimi,
`channel::post`), toplayıcı registry diliminde `joins_unsent` sayar
(`gsb_registry_joins_unsent_total`). `joins_unread`'in kapalı eşi: bir
katılma ya kutuda kalır ya kutu onu reddeder, ikisi birden değil — katılma
hattının her durağı artık kaybını sayıyor (B57 `join_ops_dropped`, B75
`joins_refused_closed`, B68 `joins_unprocessed`/`resumes_unprocessed`,
F53 `joins_unread`, F54 `joins_unsent`). Registry dilimi, çünkü sebep
registry'nin duruşu; bağlantının net örneği değil (F53'ün sayacıyla yan
yana okunur). Kilit: `conn_counts::stopped`.

**Panikleyen oda/shard (B67).** Ölüm bekçisi (oda/shard görevi başına bir
görev, yalnız `JoinHandle`'ı bekler) registry'ye `RoomDied` bildirir;
registry güncel enkarnasyonu biçer (üyelere `RoomGone`, `rooms_died`).
B67'den beri: (1) biçim kaydı `stop_room` ile durdurur — sharded odanın
HAYATTA kalan shard'ları da `Shutdown` alır ve son sayımlarıyla biter
(önceden sunucu durana dek çalışıyorlardı); (2) bekçi görev panikle (ya da
iptalle) bittiyse toplayıcıya görevin satır kimliğiyle
`MetricsEvent::RoomEndedUncounted` gönderir: son sayım yok, sayılır
(`rooms_ended_uncounted`), satır budanır — ayrıntı §12 "Panikle ölen
oda/shard".

### 9.1 Kapanış kilitlenmesi (S turu, BACKLOG §1 satır 4a)

**Belirti.** U turunda bir MMO ve üç arena-500 loadgen koşusu bitmedi:
`stop()` asılı kaldı. Geçici logla registry'nin bir odanın kontrol
kanalında `cap=0` ile beklediği görüldü. Hata U'dan eski; rUDP turu
yalnızca daha çok eşzamanlı kopuş ürettiği için yüzeye çıkardı.

**`stop()` yolunun tamamı, her `.await` ile** (düzeltme öncesi):

1. `stop()`: `registry.send(Shutdown).await` — registry posta kutusu
   (4096); registry onu tick'ten bağımsız boşaltır.
2. `stop()`: `http.abort()`, `ticker.abort()` — beklemesiz (B33'ten
   beri ops yüzeyi abort edilmez: kapısı kapanır, döngüsü accept
   döngüleriyle beklenir, yukarıda). Ticker
   görevinin göndericisi düşer, ama registry bir `Ticker` klonu tuttuğu
   için broadcast **açık kalır**; odalar artık tick almaz, yani kontrol
   kanallarını bir daha boşaltmaz.
3. `stop()`: her listener `close()`, her accept `abort()` — beklemesiz
   (B16'dan beri abort yok: `close` döngüyü bitirir, yukarıda).
4. `stop()`: `metrics.await` — toplayıcı yalnız broadcast `Closed`
   görünce biter; broadcast da ancak registry çıkıp `Ticker`'ını
   düşürünce kapanır. `stop()`'un tamamlanması bu tek zincire bağlı.
5. Registry, Shutdown'dan ÖNCE kuyruğundakileri işler (kopan
   istemcilerin `ConnClosed`'ları, ops yüzeyinden bir `DestroyRoom`):
   her `ConnClosed` dispatcher'a (ya da spawn'lu bir göreve) bir DETACH
   yollatır; bunlar odanın kontrol kanalına `send().await` ile yazar —
   kendi görevlerinde, registry'yi bekletmeden.
6. Registry `on_shutdown`: (a) dispatcher'lara `try_send(Close)`; (b)
   bağlantılara spawn'lu `ConnIn::Shutdown`; (c) **her odaya satır içi
   `control.send(Shutdown).await`, shard'lı odada her shard'a
   `ShardMsg::Shutdown`** — KİLİTLENME BURADA; (d) dönüş → `break` →
   registry ve `Ticker`'ı düşer.
7. Oda/shard: tek await'i `tick_rx.recv()`; `Closed`'da
   `logic.on_shutdown()` + `match_result` (`try_send`) → çıkar →
   `control_rx` düşer, kanala bekleyen her gönderici hata alıp biter.
8. Bağlantı aktörü `ConnIn::Shutdown`'da döngüden çıkar (B12'den beri
   önce ERROR 14'ü beklemesiz `try_send` eder — §5.6; bu zincire await
   eklemez), son metrik örneğini `try_send` eder,
   `registry.send(ConnClosed).await` (registry çıktıysa anında hata).

**Kök neden.** Stop anında kontrol kapasitesinden (varsayılan 128) fazla
üye koparsa 5. adımın DETACH'leri kanalı doldurur ve fazlası kanalda
bekler. 6c'deki `send().await` FIFO sırasında onların arkasına girer;
kanalı boşaltacak tek şey odanın tick'i, tick de 2. adımda durdu.
Registry sonsuza dek bekler, `Ticker`'ı düşmez, broadcast kapanmaz, oda
`Closed`'u hiç görmez, toplayıcı bitmez, `stop()` asılı kalır. Kapalı
bir döngü: odanın ilerlemesi, onu bekleyen registry'nin elindeki
`Ticker`'ın düşmesine bağlı.

**Aynı sınıftan ikinci bekleme.** `on_destroy_room` aynı satır içi
`send(Shutdown).await`'i taşıyordu. Shutdown'ın önüne kuyruklanmış bir
`DestroyRoom` (HTTP ops ya da `close_room`, stop'la yarışan), ticker
çoktan durmuş ve kanal doluyken registry'yi Shutdown'a varmadan
kilitliyordu. Ticker çalışırken de, tüm kontrol düzlemini kuyruktaki
mesaj başına kapasite kadar tick bekletiyordu. Diğer bekleme yerleri
tek tek incelendi, **değişmedi**: dispatcher'ların ve spawn'lu
leave/detach görevlerinin oda gönderimleri kendi görevlerinde; oda
çıkıp alıcıyı düşürünce biterler. Spawn'lu `ConnIn::Shutdown`
gönderimleri tick'e bağlı değil. Bağlantı aktörünün ve ölüm
bekçisinin (`RoomDied`) registry'ye gönderimleri, registry çıkınca
anında hata alır. Hiçbiri `stop()`'un zincirinde değil. Düzeltmeden
sonra registry kolu içinde oda posta kutusunu bekleyen satır içi
`.await` kalmadı: registry'nin tek beklediği kendi posta kutusu.

**Düzeltme** (`registry/actor/stop.rs`): durdurma yolları (sunucu
kapanışı ve destroy) oda posta kutusunu **hiç beklemez**. `post_stop`,
yer varsa mesajı `try_send` ile yerinde bırakır. Kanal DOLUysa mesajı
spawn'lu bir göndericiye devreder (leave/detach yollarının zaten
kullandığı fire-and-forget deyimi). Alıcı yoksa (oda çoktan ölmüşse)
hiçbir şey yapmaz. `on_shutdown` ve `on_destroy_room` artık senkron.
Sunucu kapanışında registry hemen çıkar ve `Ticker`'ını düşürür;
ticker da iptal edilmiş olduğundan broadcast kapanır, her oda
`Closed`'dan çıkar ve teardown kancalarını (`on_shutdown`,
`match_result`) işlenmiş bir Shutdown'daki gibi koşar. Bekleyen spawn'lu
gönderici, düşen alıcıya çarpıp biter; odadan uzun yaşayan görev
kalmaz. Ticker çalışmaya devam ediyorsa (çalışma anında destroy ya da
registry'yi ticker'ı iptal etmeden durduran kütüphane kullanıcısı), oda
kanalı boşaltır ve spawn'lu gönderici Shutdown'ı SIRAYLA teslim eder.

**Korunan sözler.** Odalar yine durur. Sıra şu: kanalda yer varsa
Shutdown, yoksa ve ticker durmuşsa `Closed`; iki yol da aynı
`on_shutdown` + `match_result`'u koşar. Eski sıralama da bunu çoğunlukla
fiilen `Closed`'a bırakıyordu, çünkü ticker registry Shutdown'ı işlemeden
iptal ediliyordu. Park/despawn kancaları değişmedi: kapanıştan önce
tick'te işlenen DETACH'ler kancalarını koştu. Kanalda kalanlar, eskiden
de olduğu gibi, oda `Closed`'dan çıkınca düşer. Metrik tarafında yeni
kayıp yok. Toplayıcının son raporu aynı mekanizmayla (broadcast
kapanışı) basılır; §12'nin "kapanışta kümülatif sayaçlar geride
kalabilir" sınırlılığı aynen geçerli. *(F35'ten beri son rapor
broadcast kapanışında değil, oturum üreticilerinin hepsi bittikten
sonra basılır — §12 "Son rapor üreticileri bekler".)* İstemci tel baytları değişmedi.
Kapanışta istemciye bildirim (B12) bu turun konusu değil; bağlantılar
yine sessizce kapanır.

**Elenen alternatifler.**

1. *Yalnız `try_send`, doluysa mesajı at (broadcast geri sigortasına
   güven).* Sunucu kapanışını çözer, ama ticker'ın çalışmaya devam ettiği
   her yolu bozar: dolu kanallı bir oda çalışma anındaki destroy'da ya
   da registry'yi ticker'ı iptal etmeden durduran kütüphane
   kullanıcısında hiç durmaz. Spawn'lu yedek bu yüzden var. Mutasyon
   testi (`full_mailbox_still_gets_the_message_in_order`) tam bu
   gerilemeyi yakalar.
2. *`stop()`'ta ticker'ı ancak registry bitince iptal etmek* (`stop()`
   `RegistryTask`'ı bekler). Sağlıklı odalarda kilidi açar, çünkü odalar
   tick'lemeye devam edip kanalı boşaltır. Ama bekleme oda sağlığına ve
   bekleyen trafiğe bağlı ve sınırsız: 1 Hz'lik bir oda, 500 kopuş /
   128 kapasite → oda başına 4+ sn, odalar arasında seri. Zaman aşımı
   eklense de aşımda registry yine `Ticker`'ı tutarak asılı kalır; kilit
   gitmez, yalnız bir zaman aşımının arkasına saklanır. Düzeltmeden sonra
   sıra değişikliği gereksiz.
3. *Oda `Closed`'da kontrol kanalını boşaltıp çıksın.* Kilidi tek başına
   çözmez: registry hâlâ bekler, broadcast hiç kapanmaz, oda `Closed`'u
   hiç görmez. Ayrıca senkron boşaltma yalnız kuyruktakileri görür (en
   çok kapasite kadar). Kanalda bekleyen göndericiler, oda yield etmediği
   için hiç giremez. Böylece koşulan park kancaları keyfi bir alt küme
   olur. Üstüne ölmekte olan odada Join kabulü koşar.
4. *Registry `Ticker`'ını kapanışın başında düşürsün.* Sunucu stop'unu
   çözer, ama doğruluğu kompozisyon kökünün iptal sırasına bağlar. Destroy
   yolunu (Shutdown'dan önce kuyruklanmış `DestroyRoom`) çözmez, alanı
   `Option`'a çevirir. "Registry bir odayı asla beklemez" kuralı daha
   dar ve yerel.
5. *Daha büyük/sınırsız kontrol kanalı.* Sınırlı-kanal kuralını (§2)
   çiğner; eşiği yalnızca taşır.

**Testler.** `gsb-core/tests/shutdown.rs` en kötü iç içe geçmeyi
deterministik olarak kurar: ticker ÖNCE iptal edilir, 12 üye kopar
(kapasite 2), DETACH'ler kanalı doldurup beklemeye geçer, sonra
Shutdown/destroy gönderilir. Tek oda, shard'lı oda (2 shard) ve destroy
için 3 test var. Her biri registry'nin bittiğini, broadcast'in
kapandığını ve her oda/shard'ın teardown'unu koştuğunu (`match_result`)
doğrular. `gsb-server/tests/server_stop.rs` aynı durumu gerçek TCP ile
uçtan uca kurar (10 Hz, `room_control = 2`, 24 üye, kopuştan ~150 ms
sonra `stop()`, tek oda + shard'lı), `stop()`'un 10 sn içinde
bittiğini doğrular. Düzeltmesiz kodda beşi de 5/5 koşuda kilitlendi.
`post_stop` birim testleri dolu kanalda mesajın kaybolmadığını pinler.
`gsb-core/tests/shutdown/destroy.rs` ticker ÇALIŞIRKEN sharded bir odanın
çalışma zamanında yok edilmesini kilitler: her shard teardown'unu tam bir
kez koşar — düz durumda ve posta kutuları DETACH'lerle doluyken (4 Hz,
kapasite 2; registry hemen döner, spawn'lu gönderici shard'lar kuyruğu
eritince Shutdown'ı teslim eder). Ebeveynin bağımsız mutasyonu (sharded
kolu hiçbir şey göndermez) ilk hâlde hiçbir testi kırmıyordu — tam
sunucu durdurmada zararsız, runtime destroy'da shard sızıntısı; bu iki
test o boşluğu kapatır.
Loadgen (TCP, süreç içi, 500 istemci): `--game arena
--write-stall-secs 0` ve `--game mmo` 4'er koşu. Hepsi bitti,
`joined = left = 500`, `errors=0`. Düzeltmesiz kod da 2+2
karşılaştırma koşusunda bitti (kilit zamanlamaya bağlı ve varsayılan 128 kapasitede seyrek), yani
loadgen ayırt edici test değil; ayırt eden deterministik testler.

### 9.2 Servislerin açık durdurması (BACKLOG F5)

**Boşluk.** Bir oyun servisi (demo'nun ekonomi servisi: odaların
yanında koşan, tutamaç klonuyla beslenen uzun ömürlü görev) yalnız bütün
göndericileri düşünce biterdi. `stop()` açısından bu örtük ve sırasızdı:
bir odanın son `on_shutdown`/`match_result`'ı servisle bir işi
kapatmak (ör. ekonomi işlemini kesinleştirmek) isteyebilir, servisin o
an hâlâ ayakta olacağının ya da `stop()` döndükten sonra (runtime
düşerken) yarıda kesilmeyeceğinin hiçbir güvencesi yoktu.

**Envanter** (motor, kit, sunucu ve oyunların doğurduğu, oda/bağlantı/
dinleyici olmayan uzun ömürlü görevler):

| Görev | Nasıl beslenir | Bugün nasıl biter |
|---|---|---|
| Ekonomi servisi (`gsb-demo`, demo modülü başına bir) | sınırlı posta kutusu (64); her demo odası ve fabrika bir tutamaç klonu tutar; `ECONOMY` isteği `External` future'ı → RPC işçisi → `buy()`; istek başına kısa cevap görevi | son gönderici düşünce (F5'ten sonra: kaydedilince açık `Stop`) |
| Ticker | — | `stop()` abort eder |
| Metrik toplayıcı | sınırlı metrik kanalı (oturum) + taşıma kanalı | broadcast `Closed`, sonra oturum kanalının kapanışı ya da `FINAL_REPORT_GRACE` (F35; `stop()` bekler) |
| HTTP ops accept + oda defteri | ops bağlantıları; defter kanalı | `stop()` kapısını kapatır, döngü döner (B33; abort yalnız geri sigorta); defter göndericisi düşünce |
| Registry, ölüm bekçileri, RPC işçileri | kendi posta kutusu; oda `JoinHandle`'ı; `request_timeout` (5 sn) | `Shutdown`; oda bitince; süre sınırı |

Arena'da servis yok; MMO/war'ın `Realm`'i bir `Arc` veri, görev değil;
kit'te servis yok (`record_run` rig'i yalnız test). *Odaların kapanış
yolunda servise gönderdikleri:* in-tree oyunların `on_shutdown`/
`match_result`'ı servise hiçbir şey göndermez; gönderen tek yol, kapanıştan
önceki tick'lerde `External`'a devredilmiş ve işçisi hâlâ uçuşta olan
bir alımdır (işçi `request_timeout`'a kadar bir tutamaç klonu tutar).
Sözleşme yine de "oda çıkarken servise yazabilir" durumunu hedefler —
yapı taşı bunun için.

**Yapı taşı** (`gsb_core::service`, `gsb-server` `RegistryParts`):

1. *Düşme bariyeri* — `service::hold()` → klonlanabilir `Hold` + tek
   `Released`. `Released::wait` son `Hold` düşünce biter (tek await: hiç
   değer gönderilmeyen — öğe tipi `Infallible` — bir kanalın `recv`'i).
   `Registry::with_rooms_hold(hold)` ile registry bir token tutar ve her
   oda/shard ölüm bekçisine bir klon verir; bekçi odanın `JoinHandle`'ı
   bitince (teardown kancaları koşmuş, dünya düşmüş) token'ı bırakır.
   Registry'nin await kümesi değişmedi: bekleyen bekçiler zaten vardı,
   token onlara biner; registry hiçbir oda posta kutusunu beklemez.
2. *Servis* — `service::Service::new(ad, görev, durdur)`: `durdur`
   senkron bir `FnOnce`; tipik gövdesi servisin kendi `Stop` mesajını
   `gsb_core::channel::post` ile göndermek (registry'nin `post_stop`
   deyimi, artık public: yer varsa `try_send`, doluysa spawn'lu gönderici,
   alıcı yoksa hiçbir şey). Bant içi `Stop`, aktörlerin `Shutdown`
   deyimiyle aynıdır: FIFO sayesinde kuyruktaki her şey ondan önce işlenir.
3. *Kayıt* — modül `spawn_registry` içinde, registry'yi başlatmadan önce
   `parts.service(s)`; `RegistryTask` servisleri taşır, `ServerHandle`
   tutar. **İsteğe bağlı:** kaydedilmeyen (ya da `Service`'i düşürülen)
   bir servis eski hayatını sürer; `ServerHandle` `stop()`'suz
   düşürülürse `Service`'ler düşer, istek gönderilmez.
4. *`stop()` sırası* — `Shutdown` → HTTP ops kapısı `close` (B33) →
   ticker abort → dinleyiciler `close` → accept döngüleri, ops'unki
   dahil (≤ 1 sn, B16) → odaların
   bariyeri (≤ `SERVICE_STOP_GRACE` = 1 sn) → her servise istek (hepsine
   birden) → hepsi TEK bir son tarih (≤ 1 sn) altında join, aşan abort →
   metrik toplayıcı. En kötü ek süre 2 sn; in-tree'de odalar bir tick
   içinde, ekonomi gecikmesi (5 ms) içinde biter. `StopReport` üç alan
   kazandı: `rooms_finished` (bariyer süresinde açıldı mı; `false` ise
   servisler yine durdurulur, geç kalan odanın mesajı `Stop`'un arkasına
   düşüp kaybolabilir — `warn`), `services_ended`, `services_aborted`
   (ikincisi `warn`).

**Ekonominin benimsemesi.** `EconomyService::start(gecikme)` →
`(tutamaç, Service)`; posta kutusu artık `Buy | Stop` taşır (özel tip;
`EconomyBuy` ve `buy()` aynı). `Stop`'ta işçi döngüden çıkar, alıcıyı
düşürür (sonraki istek "economy service gone" alır), sonra borçlu
olduğu cevapları bekler: her cevap görevi bir `Hold` tutar, işçi
bariyeri döngüden SONRA, sıralı bekler (döngünün tek await'i yine
`recv`). `EconomyService::spawn` = `start(..).0` — eski hayat. Demo
modülü ekonomisini kaydeder; `accept_stop`'un beklediği rapor artık
`services_ended = 1`.

**Korunan kurallar.** `stop()` her zaman biter (üç süre sınırı, her
biri tek bir join/wait'in sınırlı beklenişi — B16 deyimi); registry
hiçbir oda posta kutusunu beklemez; istemciye bağlı await yok; aktörlerde
select yok (servisin tek await'i `recv`, durdurma bant içi); kilit yok;
istemci tel baytları değişmedi.

**Elenen alternatifler.**

1. *Yalnız join* (servis düşme kuralıyla biter, `stop()` sadece sınırlı
   bekler). Sıra yalnız servis tutamaçlarını YALNIZ odalar tuttuğunda
   doğru: uçuştaki bir RPC işçisi klonu 5 sn'ye kadar, modülün sakladığı
   bir klon ya da servisin kendi alt görevleri sonsuza dek tutar — her
   `stop()`'ta abort. Açık istek sahiplikten bağımsızdır.
2. *İptal jetonu* (`run_until_cancelled(rx.recv())`). Servis döngüsüne
   ikinci bir kaynak ekler ve kuyruğu keser: odaların son mesajları ya
   kaybolur ya da ayrıca boşaltılmalıdır. Bant içi `Stop` aynı sırayı
   FIFO'dan bedava alır.
3. *Registry odaların `JoinHandle`'larını beklesin.* Registry'nin tek
   await'i kendi posta kutusudur ve `Shutdown`'da hemen çıkar (§9.1);
   bekçiler zaten o handle'ları bekliyor.
4. *Token'ı oda aktörlerine vermek.* İki aktör tipine (oda, shard)
   dokunur; bekçiler ikisi için tek yer.
5. *Servisleri kayıt sırasıyla, biri bitmeden ötekine istek göndermeden
   durdurmak.* Servisler arası bir sıra verir, ama tek son tarih altında
   takılan bir servis sonrakileri hiç istek almadan abort'a iter.
   Servisler arası bağımlılık ihtiyacı doğarsa ayrı yapı taşı.
6. *Oda + servis için tek son tarih.* Odalar süreyi yerse servisler hiç
   süre alamadan abort edilir; iki ayrı pencere.
7. *`GameModule`'e `fn services()`.* Servisler `spawn_registry` içinde
   doğuyor; kayıt da orada, generic'siz trait'e durum taşımadan.

**Testler.** `gsb-core/tests/rooms_released.rs` — iki tek oda / iki
2-shard'lı oda, `on_shutdown` 150 ms bloklar: bariyer ancak bütün
teardown'lar düştükten ve registry bittikten sonra açılır.
`gsb-core` `service::tests` — bariyer son token'da açılır; `Service`
düşürülünce istek gitmez, görev yaşar. `gsb-server` `boot::stop::tests` —
servislere istek odalar bittikten sonra gider; tek son tarih altında iki
sağır servis abort, biri biter; süreyi aşan odalar servisleri tutmaz
(`rooms_finished = false`). `gsb-server/tests/service_stop.rs` — uçtan
uca: iki odalı test oyunu, her oda `on_shutdown`'da 200 ms bloklayıp
defter servisine `Settle` yollar; `stop()` döndüğünde olay sırası
"oda kapandı → yerleşti" (her oda için) ve en sonda "servis durdu";
rapor `rooms_finished`, `(1, 0)`; sağır ikinci servisle `(1, 1)` ve
`stop()` ~1 sn'de biter. `gsb-demo` `economy::tests` — `Stop`'tan önce
kuyruğa giren alımlar cevaplanır ve görev ancak cevaplar çıktıktan sonra
biter; `Stop`'tan sonraki alım reddedilir; `spawn` eski hayatı korur.
Mutasyonlar (hepsi en az bir testi düşürdü): bekçinin token'ı
`handle.await`'ten önce bırakması; `with_rooms_hold`'un token'ı
tutmaması; tek oda ya da shard bekçisine token verilmemesi (ayrı ayrı);
`RegistryParts::spawn`'un bariyeri registry'ye vermemesi; servislere
isteğin oda beklemesinden önce gitmesi; `stop()`'un servisleri hiç
durdurmaması; aşan servisin abort yerine "bitti" sayılması;
`rooms_finished`'in hep `true` olması; ekonominin borçlu cevapları
beklememesi; `Stop`'u yok sayması; demo modülünün ekonomiyi kaydetmemesi.

## 10. v1 kısıtları

| Kısıt | Neden | Yol |
|---|---|---|
| ~~Yayın = tam snapshot (grup başına)~~ *(kapandı — delta modu: AOI/`spatial` (§8.1 "Delta yayın"), sharded × spatial, takım sisi odası `TeamRoom::with_delta` (T) ve team × sharded `ShardedTeamRoom::with_delta` (W1); açık, PVS ve düz sharded odalar full; zarf ve istemci kuralları `crates/gsb-kit/proto/kit.proto`)* | Full kendi kendine yeter; delta modunda yakınsama keep-alive full'ıyla | değer düzeyinde delta — BACKLOG A22 |
| Keepalive snapshot'ı (varsayılan 1 Hz) | Son paketi kaybeden istemci kalıcı bayat kalmasın | `keepalive_hz` (tick hızını aşamaz: oda kendi tick hızından hızlı keepalive yapamaz; yüksek değer `KeepaliveRate` ile reddedilir); 0 ile kapatılabilir |
| `max_snapshot_bytes` aşımında yalnızca uyarı (grup başına bir kez) + `snap_overflows` sayacı | Payload çekirdekte bölünmez; rUDP'de eşiği aşan kare **taşımada parçalanır** (§6 "MTU"), yani sayaç artık bant genişliği/parçalanma sinyali — kayıp sinyali istemcinin `frag_dropped_incomplete`'i | uyarıya göre grubu böl (AOI) / hızı düşür (§8) |
| Oda hizi global tick hızını tam bölmeli | broadcast ticker + adım atlama (`run_every`) | global hız tek kaynak; dinamik adaptif tick gelecek |
| ~~Accept loop abort~~ *(kapandı — B16: `close` bekleyen accept'i bitirir, döngü kendiliğinden döner; abort yalnız 1 sn'yi aşan döngüye geri sigorta)* | — | §9 |
| rUDP: **tıkanıklık tepkisi opt-in, oyuna sinyal yok** *(B1 tur 3: `udp_congestion = "pace"` — raporlayan oturumun oyun bandı tahmini yol hızına göre hızlanır, en eski kareler düşer + sayılır; varsayılan `"off"`)* | Varsayılan kapalı: titreşimli gerçek yolda sahte gecikme sinyali ölçülmedi; `PathState` henüz odaya ulaşmıyor, oyun içeriğini yola göre inceltemez | sinyal çekirdek/kite (sonraki tur — §6 "Tıkanıklık tepkisi"), sonra varsayılanın çevrilmesi |
| rUDP: ~~şifreleme/imza yok~~ — **B5a'da kapandı** | Mühürlü kapı (sunucu varsayılanı): Noise NK + ChaCha20-Poly1305 kayıt katmanı, sunucu statik anahtarı config'de (§6 "Kayıt katmanı", `docs/RUDP-SECURITY.md`). Düz metin yalnız açık `udp_security = "plaintext"` (dev/LAN); orada çerez anahtarı tahmin edilemezdir ama hiçbir şey imzalanmaz ya da şifrelenmez | B5b'de (2026-10-03) rekey politikası ve stateless reset yapıldı (§6 "Anahtar fazları ve stateless reset"); kalan: CID rotasyonu (tasarım RUDP-SECURITY §10), dış inceleme (D13, kapsam RUDP-SECURITY §11) |
| rUDP: parçalama **yalnız oyun bandında, yalnız sunucu → istemci**, mesaj başına en çok 16 parça (varsayılan bütçede 23 472 B); aşan kare atılır + sayılır; kontrol bandı parçalanmaz (aşan kontrol karesi oturumu bitirir) | ölçülen en büyük full 10 267 B (arena 1000; W2'de savaş 1000'in keep-alive full'ü ~18,5 KB — CROSS-SHARD §8b.8); yeniden gönderim yok — bant kendini iyileştirir; istemci durumu sabit sınırlı (§6 "MTU", SECURITY §4.1) | daha büyük kareler için grup bölme (AOI) — §8 |
| ~~rUDP: SO_RCVBUF ayarı yok~~ *(kapandı — B4: `udp_recv_buffer_bytes`/`udp_send_buffer_bytes`, rUDP ve QUIC kapıları, `socket2` ile; yazılmazsa dokunulmaz — §6 "UDP kapılarının soket arabellekleri")* | — | — |
| rUDP: **bağlantı göçü opt-in** *(B3: `udp_migration = true` — CID + yol doğrulaması, NAT yeniden bağlanması ve ağ değişimi oturumu bitirmez; varsayılan kapalı: yeni el sıkışma + resume, eski oturum idle sweep'e kadar)* | Kriptodan önce CID taşıyıcı jetondur: koklayan, challenge'ı yanıtlayıp s→c akışını kendine çekebilir (RUDP-SECURITY §3, §7) | B5a (mühürlü kayıt) — sonra varsayılan açık |
| Oda kapasitesi **vardır**: `max_players` (vars. `Some(10_000)` = ölçülen duvar) + sunucu geneli `max_connections` (vars. `Some(100_000)`) | koruma katmanı (bu tur); semantiği: nazik reddi — oda dolu `ERROR 8` (bağlantı yaşar), cap `ERROR 9` + kapatma; çünkü sınır, ölçülen sayılara dayandı (C1 duvarı 9–10k), tahmine değil | sınırsız oda gerekirse `None` (0 = sınırsız) |
| join/leave tick sınırında işlenir (≤ 1 tick gecikme) | CONTROL fazı determinizmi (bilinen tick'te spawn/leave) | v1'de kabul edilen özellik; gerekirse tick-içi hızlı yol |
| Girdi kaybı **yalnızca göndericinin kendi kanalında** ve **atfeli**: connection actor `try_send` Full'u kendi metrik örneğinde sayar (`actions_dropped`, `actions_dropped_top`); odaya çeken READ fazı sınırlı çekmedir — bağlantı başına tick bütçesi 16 + oda çekme bütçesi 65536, oda çektiği aksiyonu asla atmaz | flooding bir bağlantı başkasının aksiyonunu evicted edemez (eski merged-list en eskiyi atıyordu); hasar saldırgana sınırlı | ~~sürekli (sn başına) rate-limit~~ opt-in olarak var (E1, §4 "Girdi HACMİ"): aşan girdi göndericinin aktöründe düşer, `input_rate_limited` sayılır |
| Tek process | v1 kapsamı | ~~§8.4~~ §8 "sonraki adımlar" madde 4; süreç içi bölme §8.2; süreçler/makineler arası: DISTRIBUTED (`ShardLink` tasarımı) |
| Oturum zaman aşımı **reader pump'ta** (read deadline), registry'de değil | çünkü saati tutan yer, stream'i bekleyen yeridir — registry'ye son-görülme damgası ikinci bir beklenen kaynak/timer çıkarırdı (§3); 30 sn varsayılan, 0 = kapalı | oyun seviyesi oturum politikası (reconnect'de yeniden auth vb.) registry katmanı |
| `sint32` (tam sayı) koordinat, `f32` simülasyon | Demo sadeliği | float veya mm cinsinden int (sabit nokta) |
| ~~Güvenlik yüzeyi minimal: AUTH no-op~~, ~~sn-başına **geçerli girdi** hacim sınırı yok~~ *(E1: opt-in token bucket `RoomConfig::input_rate`, varsayılan kapalı, sayıyı oyun/config verir — SECURITY §3.4)* (cap'ler var: bağlantı cap + oda cap + tur-başına girdi bütçesi) *(AUTH kısmı kapandı — ticket kancası `TicketAuth` (`gsb-core/src/auth.rs`; yapılandırılmazsa `Auth.name` olduğu gibi kabul: yalnız geliştirme yolu, SECURITY §4b), AUTH deneme sınırı + pre-auth kare bütçesi + HEARTBEAT kısması + unauthed cap (SECURITY §3–§4), TLS (SECURITY §2))* | saniyede kaç aksiyonun meşru olduğu oynanış parametresi — kullanıcı kararı (2026-09-27: opt-in yapı taşı) | ~~`Authenticator` trait'i + rate-limit~~ — kapandı (E1) |

> Not: Önceki sürümlerdeki iki kritik hata — sonradan giren oyuncunun
> dünyayı görmemesi ve registry'nin oda cevabını beklerken tüm sunucuyu
> bloke etmesi — kapatıldı (§3, §7). `RoomId(0)` sentinel'ı kaldırıldı,
> `ConnInfo.room` artık `Option<RoomId>`. Koruma katmanı turunda kapatılanlar:
> oturum yaşam döngüsü (yarım açık TCP, reader-pump read deadline — §3),
> oda kapasitesi + sunucu bağlantı cap'i (nazik reddi, ERROR 8/9 — §5),
> girdi adaleti (sınırlı çekme + saldırgana atfeli drop — §4).

## 11. Test stratejisi

- **gsb-protocol:** frame encode/decode, bozuk çerçeve, tablo round-trip,
  bilinmeyen opcode.
- **gsb-lint:** yorum soyma (satır/blok/iç içe), satır numarası korunumu.
- **gsb-net:** gerçek loopback TCP üzerinde framing round-trip, çoklu
  frame yeniden derleme + EOF, aşırı boyutlu length-prefix reddi.
  (Testler echo-peer kullanır; pasif peer'da TCP yarı kapanışı davranış
  farkı yaratır.) Koruma katmanı: **reader-pump idle timeout** — sessiz
  peer pencere döneminde `ServerClosed` alır (reason'de "idle timeout"),
  aktif peer (tempolu poke'lar — kontrol edilebilir `mpsc`-arkalılı `Stream`
  adaptörü, TCP'nin buffer'ı tempoyu yutmasın diye) pencereyi sıfırlar ve
  hayatta kalır, peer tarafı EOF ise `peer closed` olarak raporlanır — idle
  olarak değil.
- **gsb-core:** global ticker + oda actor — tick fazları, `dt` üst sınırı
  (catch-up), `run_every` ile yavaş oda atlama, `Lagged` sonrası catch-up +
  ticker kapanışında temiz çıkış (sentez zaman damgalarıyla manuel
  broadcast besleme, kilit yok). Registry: join→leave→rejoin dizisi
  (gözlemci bağlantı üzerinden oyuncu sayısı doğrulanır — stale leave
  sayacı geri düşürmemeli), oda imhası bildirimi + imha sonrası join
  reddi, bölünmeyen oda hızı reddi (`TickRate`), temiz shutdown
  (registry handle'ı çözülür) — **gerçek 60 Hz ticker** ile.
  Koruma katmanı: oda doluyken join reddi — reply `CoreError::RoomFull`
  taşır, kalan üyenin girdisi etkilenmez; **flooding bir bağlantı başkasının
  aksiyonunu evicted edemez** — sınırlı çekme: kurbanın 20/20 aksiyonu
  içerilir, flood backlog'u flood'un **kendi** kanalında birikir (salın
  slot sayısı ile kanıt: tam olarak 8/tick çekme miktardaki kadar slot
  açılır); registry: dolu odada spawn reddi + bağlantı cap'inde
  `ConnOpened` → `ServerClosed` (bağlantı tabloya kaydedilmez, `ConnClosed`
  temiz no-op — JOIN’in `ServerClosed` ile yarışmasının köşesi).
  Grup mekanizması: `GroupKey = ConnectionId` mantıkla gruplar birbirinden
  yalıtılır (bir grubun snapshot'ı başka bağlantıya asla sızmaz), private
  frame yalnızca hedef bağlantıya gider; değişmeyen grup sessiz kalır,
  keepalive kadansında önbellekli snapshot yeniden gönderilir; **her tick
  değişen dünyada aynı tick'te değişen her grup yayınlanır** (grup başına
  defterle — oda tarafının grup-başına davranışına regresyon; mantık
  tarafındaki paylaşımlı defter yanlış kullanımı bu testle yakalanamaz:
  oda, meşru sessizlik ile ihlali ayırt edemez, o kullanım sözleşme
  metniyle korunur — §4 Tanı maddesi).
- **gsb-demo:** gecikmeli giriş — hareketsiz A'nın olduğu odaya B girerse B,
  aynı tick'in `WORLD_SNAPSHOT`'ında **A dahil tüm dünyayı** görür; A
  hareket edince B, sonraki snapshot'larda yeni konumu görür (üyelik ve
  durum snapshot'ta varlıkla/val ile ifade edilir). Stale leave —
  rejoin'dan gecikmeyle gelen eski leave, yeni entity'yi öldüremez
  (hedefli MOVE_TO + snapshot ile doğrulanır; snapshot'ta yeni entity var,
  eski entity yok). Kare hızından bağımsızlık — tek 60 Hz saat altında iki oda
  (60 Hz `run_every=1` ve 15 Hz `run_every=4`), gerçek hareket sistemi:
  5.0 s simülasyon süresi her iki odada aynı mesafe (f64 gözlem kanalı —
  i32 wire, karşılaştırmayı kuantum gürültüsü altında boğardı). Snapshot
  "değişiklik yok" kararlayıcısı (2 unit test) — demo kararı yalnız wire
  içeriğiyle (entity kümesi + kesilmiş konumlar) alır: düz bir
  `Position` yazımı yayınlanır, içeriği değiştirmeyen bir yazım
  yayınlatmaz, üyelik değişimi yayınlatır (karar, denetim turunda
  kaldırılan `EntityVersion`/`bump()` mekanizmasından bağımsızdır — §7).
   Kimlik değişmezi — bevy slotu geri dönse bile (129 join/leave
   döngüsüyle zorlanır) tel kimliği yeniden kullanılmaz, dolayısıyla
   kayıp snapshot'ı olan istemci "aynı entity hareket etti" ile "yeni
   entity slotu devraldı"yı snapshot'lardan ayırt eder
   (`wire_identity_survives_ecs_slot_reuse`; §8). Yayınlanabilirlik —
   `on_join`'den geçmemiş, `Position` taşıyan bir entity (mermi/NPC/
   tuzak gibi oyuncuya bağlı olmayan entity) sessizce görünmez kalmaz:
   broadcast geçişi ona aynı sayaçtan taze wire kimliği damgalar ve bir
   sonraki snapshot'ta görünür (ön koşul yapısal — §7, "Yayınlanabilir
   küme"; `entity_spawned_outside_on_join_is_broadcast_with_fresh_
   wire_id`).
Delta yayın (spatial) — istemci VIEW'ı ile gözlemlenen altı özellik
 (`tests/delta_aoi.rs`): delta akışı ile full akışı aynı istemci
 görünümünde **yakınsar** (60-tick pencere, keepalive full dahil);
 hücre değiştiren entity, **her** istemci pozisyonunda (iki hücreyi de
 gören / kaynak-tek / hedef-tek) hayaletsiz ve kopyasız — tick tick
 değişmezi; hücre grubun görüşünden çıkınca entity'ler istemci
 tarafında **gerçekten silinir** (tek `CellExit` kaydı, yeniden
 taşınmaz); yarıda giren istemci **tek seferlik full** ile tüm dünyayı
 görür, grup o tick'ten sonra delta'da kalır (one-shot private full);
 delta kaybı **keepalive periyodu içinde** (ölçülen sınır: kayıp bitimi
 + 31 tick) taze full ile iyileşir; paket modu wire'da **kendi kendini
 tanıtır** (aktif + sessiz grupta `delta` bayrağının iki modu da
 görülür). Input sıralama/onay (op 1004 `Private` oneof'u,
 `tests/input_ack.rs`): ack **monotondur ve sunucunun işlediğinden
 fazlasını asla ack'lemez** (spec'in gerekli testi); dupl girdi
 sessizce atılır (entity geri dönmez), boşluk işareti engellemez
 (yüksek-su), rejoin input oturumunu iki tarafta sıfırlar,
 numaralandırmasız (seq 0) girdi işlenir ama asla ack'lenmez.
 - **gsb-server (e2e):** process-içi sunucu (ephemeral port) + gerçek TCP
  istemci: AUTH → JOIN → MOVE_TO → `WORLD_SNAPSHOT` akışı: önce kendi
  entity'sini görür, hareketten sonra snapshot'ta konumunu **değişmiş**
  görür. Tüm yol tek test: pump → bağlantı actor → registry → dispatcher →
  oda → bevy world → hareket sistemi → grup snapshot'ı → writer pump.
  Koruma katmanı (gerçek TCP): idle bağlantı sunucu tarafından kapatılır —
  ERROR 9, EOF'tan **önce**; 250 ms'lik heartbeat'li aktif istemci 1 s
  pencereyi çok rahat geçer (3+ ACK, ne EOF ne ERROR); oda dolu → ERROR 8
  ve bağlantı **yaşar** (heartbeat hälü ACK'lanır); bağlantı cap'i → ERROR 9
  + EOF, ilk istemci etkilenmez; flooding istemcinin düşmeleri raporda
  **kendi ConnectionId'iyle** atfeli (`actions_dropped_top` — üst bağlantı
  çakışır, toplam `net.actions_dropped` ile eşitleşir).
- **gsb-core (metrik yolu):** `room_counters_flow_to_collector` — gerçek
  `RoomActor` (canlı metrik göndericisi) + gerçek `MetricsCollector`
  (kanal sink): oda actor'ünün **yerel** sayacı (drop, steps, joins,
  members, step süreleri) actor'ün *dışından*, toplayıcının raporlarıyla
  doğrulanır — yani sayacın aktörden kanalla çıktığı kanıtı
  (sentez ticker: besleme görevi, gerçek zamanlı 100 Hz). Birikim + hız
  penceresi (Δ/rapor) ve histogram sınıflandırması ayrı unit testlerde.
- **gsb-server (yük üreticisi dumanı):** `loadgen_smoke` — gerçek
  `gsb-loadgen` binary'si spawn edilir (3 istemci × 3 s, in-process
  sunucu), `RESULT` satırı ayrıştırılır: 3/3 connect+join+leave, istemci
  ve **sunucu tarafı** tick hızı 20–40 Hz bandında (konfigure 30 Hz),
  snapshot akışı + sunucu taraflı byte sayaçları > 0. Ağırlıklı koşular
  (100/500/1000) bilinçli olarak suite dışında: suite'ü yavaşlatmaz,
  flaky yapmaz (bkz. ROADMAP "metrik + yük turu").

**Adım maliyeti (release, oda actor'ünün senkron tick gövdesi):**
N hareketsiz oyuncu + her tick hareket eden 1 oyuncu, 30 Hz oda,
manuel ticker ile beslenen gerçek oda actor'ü; 100 ısınma adımından
sonra 300 adımın medyanı/ortalaması (µs):

| Oyuncu | Öncesi (dirty×bağlantı) p50 | Sonrası (grup snapshot) p50 | Kazanç |
|---|---|---|---|
| 100 | 270 µs | 15 µs | ~18× |
| 200 | 1125 µs | 34 µs | ~33× |
| 400 | 5359 µs | 73 µs | ~73× |
| 800 | 66151 µs | 140 µs | ~472× |

Öncesi modelin maliyeti O(üye × entity) idi (her tick, her bağlantının
`last_sent` defteri taranırdı) — 800'de 33 ms bütçesinin iki katı. Sonrası
modelin maliyeti O(entity + üye) ve bağlantı sayısından bağımsız
(snapshot bir kez kodlanır, referansla dağıtılır). Ölçüm probe'u commit
edilmedi; senaryo ve tablo bu commit'in doğrulamasıdır.

## 12. Metrik altyapısı (kanalla taşıma)

Sunucu içi ölçüm, §2 disiplininin uzantısıdır: **sayaçlar ait olduğu
aktörün yerel durumundadır** ve dışarı **kanalla taşınır** — paylaşımlı
durum yok, kilit yok.

```text
room actor (her adım sonunda, senkron)   ─┐
registry (olay başına: open/close/      ─┼─▶ mpsc::bounded(4096)<MetricsEvent>
          join/leave/room)               │        (try_send = senkron, await YOK)
conn actor (≤1/sn + kapanışta son)      ─┘                  │
                                                            ▼
                    metrik toplayıcı görevi (MetricsCollector)
                    tek await'i: ticker broadcast aboneliği (odalarla aynı
                    saat kaynağı); her tick'te rx.try_recv() ile boşaltır;
                    rapor süresi (vars. 1 s) dolunca MetricReport üretir;
                    ticker kapalıysa → metrik kanalını bekler (her
                    üretici bitene dek, ≤ FINAL_REPORT_GRACE; F35)
                    → son rapor + temiz çıkış
                                                            │
                    dışa açım dikişi (emit, TEK yer): önce Exporter'lar
                    sırayla (&MetricReport), sonra MetricSink (raporu tüketir)
                                                            │
        ┌───────────────────┬───────────────────────┬───────┴──────────────────┐
        ▼                   ▼                       ▼                          ▼
 MetricSink::Log     MetricSink::Channel     MetricSink::Watch         OtlpExporter (feature otlp)
 (gsb-metric k=v;    (→ uygulama/yük         (ops yüzeyi: /healthz      try_reserve → 1 yuva →
  RUST_LOG=info)      üreticisi/test)         + /metrics Prometheus,    OtlpPusher görevi →
                                              kazımada render;          OTLP/HTTP protobuf POST
                                              feature prometheus)
```

(Sink bu üç varyanttan biridir — ops yüzeyi açıkken `Watch`; exporter'lar sink'ten
bağımsız, yan yana. Ayrıntı: aşağıda "Dışa açım katmanı" ve OPS §6.)

**Neden bounded + `try_send`:** room actor'ünün tick gövdesine **hiç await
eklenmez** (spec'in sert şartı; tek await hâlâ `tick_rx.recv()`'tir).
`UnboundedSender::send` senkrondur ama sınırsız bellek tutar (toplayıcı
sarkarsa sızma); `bounded send` ise `Future` → tick içinde bekler. Çözüm
`bounded(4096)` + **senkron `try_send`** (Future değil): kanal doluyken
örnek atılır ve üreticinin `metrics_dropped` sayacı artar — kümülatif
sayaçlar için zararsız (bkz. ROADMAP "metrik düzeltme + AOI turu").
Bağlantı aktörünün örneği ise DELTA'dır: B59'dan beri aktör, sayaç
tabanını yalnız kanal örneği ALDIĞINDA ilerletir — dolu kanalda düşen
örneğin deltaları kaybolmaz, bir sonraki (en geç son) örnek onları
yenileriyle birlikte taşır; düşüşün kendisi yine `metrics_dropped`'ta
(önceden taban `try_send`'den ÖNCE ilerliyordu ve düşen örnek
deltalarını da götürüyordu; kilit `conn_counts::samples`).
Örnekler sabit boyutlu ve oda başına adımda en fazla bir tane olduğundan
4096 derinlik geniş bir marj bırakır; en kötü hâl örnek kaybıdır, tick
durdurulamaz.

**Ne ölçülür (neden):**

| Kapsam | Metrik | Sorulan soru |
|---|---|---|
| oda | `steps`, `hz` (Δadım/örnek-aralığı), `late_*` (tick gecikmesi), `step_*` + `step_hist` (adım süresi dağılımı: tick bütçesinin **oranları**, log-2 merdiven 1/128×…32×; `(1,1)` kenarı = bütçe = aşım sınırı) | konfigure hıza ulaşılıyor mu? adım bütçesinin (33 ms @30 Hz) neresindeyiz? bütçe aşılıyor mu? |
| oda | `lagged_events/ticks` | broadcast tamponu aşıldı mı (oda tick kaçırıyor mu)? |
| oda | `dropped`, `keepalive_resends` | fan-out backpressure'ı (yavaş istemci) var mı? `dropped` yalnız DOLU çıkış kanalını sayar (B32) |
| oda | `sends_closed` (kümülatif; satırda `dropped_s`'den sonra, Prometheus'ta `gsb_room_sends_closed_total`, OTLP'de `gsb_room_sends_closed`, loadgen telinde GSMI; oran göstergesi yok) | fan-out kaç batch'i **zaten kapalı** bir bağlantıya denedi? İstemci soketini kapatmış (tipik: LEAVE sonucundan hemen sonra), oda ayrılışı/kopuşu henüz işlememiş — bağlantı sonu başına tek bir değil, (o bağlantıya tick başına denenen batch) × (soketin kapanışıyla ayrılışın/kopuşun işlenmesi arasındaki tick) — ölçülen spatial demo 1000 `--workers 1` 1,6–1,8, shard'lı MMO/savaş varsayılan worker'larla 0,9–1,9 (F59); istemcinin istediği bir kare kaybolmaz. B32'ye dek `dropped`'a karışıyordu (orkestre 500'ün 116'sı; RPC-CONTROL-PLANE §8.2) |
| oda | `snapshots`, `snap_bytes_s`, `snap_bytes_max`, `shipped_bytes`/`shipped_s`, `shipped_frames`, `private_frames` | yayın yükü: kaç snapshot, kaç bayt, tepe paket boyutu (MTU/hazırlık sinyali), kaç KARE ve bunların kaçı özel (datagram taşıması bayt kadar PAKET ile de sınırlı; `shipped_bytes/shipped_frames` = ortalama kare boyu, `shipped_frames − private_frames` = fan-out'un yayın yarısı; B39'dan beri iki kare sayacı Prometheus/OTLP'de de: `gsb_room_shipped_frames_total`, `gsb_room_private_frames_total`). B57'den beri `shipped_*` yalnız çıkış kanalının ALDIĞI batch'i sayar — düşen/kapalı batch `dropped`/`sends_closed`'dadır, trafikte değil |
| oda | `groups`, `members`, `max_group`, `joins`, `leaves` | oda doluluğu ve churn |
| oda | `actions_dropped_unread`, `actions_dropped_unbound`, `requests_dropped_unbound` (kümülatif; satırda `actions_unread=` / `actions_unbound=` `team_expired=`'den sonra, `req_unbound=` `req_unread=`'den sonra; Prometheus/OTLP'de `gsb_room_{actions,requests}_dropped_*`; loadgen telinde GSML) | oda neyi İŞLEMEDEN düşürdü? Oturum bittiğinde kanalda okunmamış düz girdiler (istekler `requests_dropped_unread`'de, B36) ve READ'in bağlama çevirisinde bağlama satırı olmayan bağlantının girdisi — istekler (defter terimi) düz girdilerden ayrı (B54). READ'in çekim bütçesi hâlâ hiçbir şey düşürmez (erteler) |
| oda | `team_export_drops_full`, `team_export_drops_closed` (kümülatif, shard satırları; satırda `team_exports=`'den sonra, Prometheus'ta `gsb_room_team_export_drops_{full,closed}_total`, loadgen telinde GSNB, `RESULT`'ta savaş için; F50'ye dek tek `team_export_drops`) | takım export'unu registry'nin posta kutusu neden reddetti? DOLU: registry çalışırken yetişemedi (gerçek kayıp, A13/A25 tetiği); KAPALI: registry duruşta çıkmıştı (§9 "Duruşta takım export'u kapalı kutuya") |
| oda | `requests_undelivered`, `requests_abandoned` (kümülatif; satırda `req_late=`'den sonra `req_undelivered=` / `req_abandoned=`, Prometheus'ta `gsb_room_requests_{undelivered,abandoned}_total`, OTLP'de `_total`'sız, loadgen telinde GSMK) | oturumu biten bağlantıya borçlu kalan RPC yanıtlarından kaçı hiç teslim edilmeden atıldı, kaç dış istek oturum bittiğinde hâlâ uçuştaydı? (B53; isteğin kendisi kendi kovasında — defter terimi değil, yanıtın akıbeti. Geri konan yanıt bir kez, atıldığında sayılır.) |
| registry | `rooms`, `conns`, `opens`, `closes`, `joins`, `leaves` | bağlantı/oda sayısı ve akışı (100k hedefinin sayacı) |
| registry | `join_ops_dropped`, `close_ops_dropped`, `match_results_dropped_full`, `match_results_dropped_closed` (kümülatif; registry satırında `rooms_died=`'den sonra, Prometheus'ta `gsb_registry_*_total`, loadgen telinde GSMP) | kontrol düzlemi neyi kaybetti? Bağlantının op dağıtıcısına verilemeyen katılma / kapanış (kuyruk dolu ya da görev gitmiş), sonuç sink'inin dolu ya da kapalı olduğu için reddettiği maç sonuçları (duran oda örnek göndermez: toplayıcıya `MetricsEvent::MatchResultDropped` ile gider). B57 |
| room | `joins_unprocessed`, `resumes_unprocessed`, `leaves_unprocessed`, `detaches_unprocessed`, `migrations_in_dropped`, `effects_unsent`, `effects_unapplied`, `team_imports_unapplied`, `border_updates_unapplied` (`RoomReport::stop`; kümülatif, yalnız son örnekte; satırda `metrics_dropped=`'den sonra, Prometheus'ta `gsb_room_<ad>_total`, loadgen telinde GSMV, `RESULT`'ta `<ad>=`, fold'da SUM) | duran oda/shard oturumlarının dışında neyi elinde tuttu? Kanalda işlenmeyen bağlantı op'ları (ayrılma/taşıma ölümünde duruşun DEFTERİ: kapanıştan sonra reddedilen hiçbir yerde sayılmaz — üyeyi duruş bitirdi, F55); shard'da kurulmayan göçler, gönderilmeyen/uygulanmayan etkiler, uygulanmayan görünüm güncellemeleri. B68 |
| registry | `rooms_ended_uncounted` (kümülatif; registry satırının sonunda, Prometheus'ta `gsb_registry_rooms_ended_uncounted_total`, loadgen telinde GSMU) | kaç oda/shard GÖREVİ son sayımı olmadan (panikle) bitti — son penceresi ve elinde kalanlar hiçbir sayaçta yok? Ölüm bekçisinin `MetricsEvent::RoomEndedUncounted`'ı; satır da onunla budanır. B67 |
| registry | `team_relays_dropped_full`, `team_relays_dropped_closed` (kümülatif; registry satırının sonunda, Prometheus'ta `gsb_registry_team_relays_dropped_{full,closed}_total`, loadgen telinde GSMW) | sharded odanın takım hub'ı (CROSS-SHARD §8b.2) kaç import'u hedef shard'a kuyruklayamadı — kutusu DOLU (yetişemiyor; kaynağın sonraki export'u kümeyi yeniden taşır) mu, KAPALI (durmuş/ölmüş) mu? Önceden yalnız `team_hub_summary` log satırında, ikisi karışık. Registry ret olduğunda örneğini hemen gönderir (röle tablo değiştirmez). B72 |
| registry | `joins_refused_closed` (kümülatif; registry satırının sonunda, Prometheus'ta `gsb_registry_joins_refused_closed_total`, loadgen telinde GSMY) | kaç katılmayı (resume denemeleri dahil) oda kutusu KAPALI olduğu için reddetti — oda/shard durmuş ya da ölmüş, op'u hiç görmedi, istemci `RoomGone` aldı? Dağıtıcının `MetricsEvent::JoinRefusedClosed`'ı (registry'yi atlar: bütün sunucunun duruşunda registry önce çıkar). Alınıp duruşta düşürülen katılma odanın `joins_unprocessed`/`resumes_unprocessed`'idir, bu değil. B75 |
| registry | `joins_unread`, `team_exports_unread` (kümülatif; registry satırının sonunda, Prometheus'ta `gsb_registry_{joins,team_exports}_unread_total`, loadgen telinde GSNC; yalnız son örnekte) | registry duruşta kutusunda neyi OKUMADAN bıraktı? `Shutdown`'ın arkasında kalan katılmalar (yanıtı düştü, istemci ERROR aldı) ve canlı sharded odanın takım export'ları (shard `team_exports`'ta saydı, hub rölelemedi). Diğer türler kayıp taşımıyor (§9 "Registry'nin kutusunda kalanlar"). F53 |
Dört üreticinin de doğru-yol kilidi var (F2): oda
`room::tests::sampling`, bağlantı `conn_counts::samples`, taşıma
`gsb-net` `metrics::tests`, registry `registry::actor::tests` — dolu
kanalda düşen örnek (registry'de oda-gitti bildirimi de) `metrics_dropped`'ta
sayılır, sonraki örnek onun söyleyeceğini taşır (registry ve oda
kümülatif, bağlantı ve taşıma deltalarını tutar), KAPALI kanal düşüş
değildir; son örnekler dolu kanalda düşmez, durdurma-mesajı deyimiyle
gider (F35/F53, kilitleri `conn_counts::samples`,
`registry::actor::run::leftovers::tests`).
| registry | `close_verdicts_lost{reason}`, `leave_verdicts_lost`, `detach_despawns_lost` (kümülatif; registry satırının sonunda `close_verdicts_lost=` + gerekçe başına `close_verdict_lost_<gerekçe>=`, Prometheus'ta `gsb_registry_close_verdicts_lost_total{reason}` / `gsb_registry_{leave_verdicts,detach_despawns}_lost_total`, loadgen telinde GSNE) | odaların hangi hükümlerini (kapatma — gerekçeye göre —, ayrılma, detach-despawn raporu) sunucunun duruşu yuttu? Odanın kuyruğunda kalan, registry'nin kapalı kutusunun reddettiği, registry kutusunda okunmayan, bağlantının kutusunda duruşun arkasında kalan; her biri tam bir yerde. `server_closes{r}` + `close_verdicts_lost{r}` = kararı verilen oturum sonu. F56 |
| registry | `joins_unsent` (kümülatif; registry satırının sonunda, Prometheus'ta `gsb_registry_joins_unsent_total`, loadgen telinde GSND) | kaç katılmayı (resume denemeleri dahil) registry'nin KAPALI kutusu reddetti — registry durmuştu, katılma hiç kuyruklanmadı, istemci `ERROR` "registry gone" aldı? Bağlantı aktörünün `MetricsEvent::JoinUnsent`'i. `joins_unread`'in kapalı eşi; bir katılma ikisinden yalnız birinde. F54 |
| conn | `bytes_in/out`, `frames_in/out` (delta), `actions_dropped` (net toplam, kümülatif; B55'ten beri yalnız oyun-bandı girdisi)
| istemci başına bant; net toplam = room fan-out (baskın) + kontrol |
| conn | `actions_dropped_top` (raporda: en çok düşürmüş 5 bağlantı, `c{n}:sayı`)
| düşen girdi **kime ait** (flooding atfesi — koruma katmanı; §4) |
| net | `input_rate_limited` (kümülatif; satırda `violations`'dan sonra, aile tablosundan Prometheus'ta `gsb_net_input_rate_limited_total`, OTLP'de `gsb_net_input_rate_limited`, loadgen telinde GSME) | odanın girdi hız sınırı (E1, §4 "Girdi HACMİ") ne kadar girdiyi bağlantı aktöründe kesti? Sınır kapalıyken 0; ihlal değil, `actions_dropped`'tan ayrı (kanal hiç dolmadı) |
| net | `actions_dropped_closed`, `requests_dropped_closed` (kümülatif; satırda `input_rate_limited`'den sonra, Prometheus'ta `gsb_net_actions_dropped_closed_total` / `gsb_net_requests_dropped_closed_total`, OTLP'de `_total`'sız, loadgen telinde GSMJ) | oda üyeliği KENDİSİ bitirdikten (atma, girdi-boşta tavanı, oda kapanışı) sonra, bildirim bağlantıya varmadan iletilen kaç oyun girdisi / RPC isteği kapalı kanala çarpıp kayboldu? Odaya hiç ulaşmadıklarından bağlantı aktöründe sayılır (B51); biten üyelik başına en çok 1 (kapalı iletim bağlantıyı odadan ayırır). İstek terimi RPC defterini kapatır (RPC-CONTROL-PLANE §8.3) |
| net | `requests_dropped_full`, `requests_no_room` (kümülatif; satırda `requests_dropped_closed`'dan sonra, Prometheus'ta `gsb_net_requests_dropped_full_total` / `gsb_net_requests_no_room_total`, OTLP'de `_total`'sız, loadgen telinde GSMM) | RPC defterinin bağlantı tarafındaki iki kenarı (B55): dolu action kanalında düşen istek (önceden oyun girdisiyle `actions_dropped`'taydı — o artık YALNIZ oyun-bandı girdisidir) ve odası olmayan bağlantıya gelen istek (`ERROR 6`; `violations`'ta sayılmaya devam eder, burada defter için ayrıca bir kez) |
| net | `heartbeats_throttled_preauth`, `heartbeats_throttled_authed` (kümülatif; satırda `hb_throttled_preauth=` / `hb_throttled_authed=`, Prometheus'ta `gsb_net_heartbeats_throttled_{preauth,authed}_total`, loadgen telinde GSMN) | heartbeat kısması (SECURITY §3.2) kaç heartbeat'i cevapsız bıraktı — kimlik doğrulamadan önce (güvenlik sinyali) ve sonra (hatalı istemci zamanlayıcısı)? İhlal değil; B56'ya dek yalnız debug satırındaydı |
| net | `frames_out_closed`, `close_notices_dropped` (kümülatif; satırda `hb_throttled_authed=`'dan sonra, Prometheus'ta `gsb_net_frames_out_closed_total` / `gsb_net_close_notices_dropped_total`, loadgen telinde GSMO) | bağlantı aktörünün kontrol karelerinden kaçını yazıcısı gitmiş (kapalı) çıkış kanalı reddetti, kaç en iyi çaba kapanış bildirimi dolu kanalda düştü? (B57; `frames_out`/`bytes_out_control` artık yalnız kanalın ALDIĞI kareleri sayar — önceden gönderimden önce sayılıyordu) |
| net | `requests_unprocessed`, `actions_unprocessed`, `control_frames_unprocessed` (kümülatif; satırda `close_notices_dropped=`'dan sonra, Prometheus'ta `gsb_net_{requests,actions,control_frames}_unprocessed_total`, loadgen telinde GSMQ) | sunucu oturumu kendisi bitirdiğinde (hüküm, atma, oda yok oldu, durma, ihlal/pre-auth bütçesi, ölü çıkış yolu) bağlantının gelen kutusunda işlenmeden kalan — ve pre-auth bütçesini aşan — kaç kare vardı, türüne göre (`conn::FrameKind`)? İstek terimi RPC defterini kapatır (RPC-CONTROL-PLANE §8.3); kutuda kalanlar `frames_in`'de değil (B60) |
| transport | `udp_{requests,actions,control_frames}_dropped_full`, `udp_acks_not_forwarded`, `udp_datagrams_{oversized,malformed}`, `udp_bad_cookies`, `udp_frags_refused`, `udp_sessions_dropped_accept_full`, `udp_frames_dropped_oversized`, `udp_control_frames_abandoned`, `udp_frames_drained`, `ws_close_frames_dropped`, `ws_pongs_dropped`, `handshakes_{refused,timed_out,failed}`, `metrics_dropped`; akış pompalarının (B66) `stream_frames_unwritten`, `stream_batches_unwritten`, `stream_{requests,actions,control_frames}_dropped_closed`, `ws_control_frames_unwritten`, `ws_frames_dropped_after_close`; rUDP'nin kalanları (B66) `udp_{game,control}_datagrams_send_failed`, `udp_{acks,challenges}_send_failed`, `udp_{requests,actions,control_frames}_dropped_closed`, `udp_datagrams_no_session`, `udp_frames_unsent` ve `writer_verdicts_deferred`; kapanan kapının ve rUDP accept tarafının (B74) `handshakes_{cut,unaccepted}_closed`, `udp_sessions_dropped_accept_gone`, `udp_sessions_unaccepted_closed`; WS'nin teslim edilemeyen teardown kapanışı (B80; kodu ne olursa olsun, F66'ya kadar `ws_going_away_unsent_*`) `ws_teardown_closes_unsent_{closed,stalled}`; WS okuyucusunun kapalı kontrol kuyruğuna veremediği cevaplar (B83) `ws_{close_frames,pongs}_dropped_closed` (kümülatif, bütün kapılar birlikte; satırda `gsb-metric scope=transport`, Prometheus/OTLP'de `gsb_transport_<ad>_total`, loadgen telinde GSMR, B66'dan beri GSMT, B74'ten beri GSMX, B80'den beri GSMZ, B83'ten beri GSNA, `RESULT`'ta `transport_<ad>=`) | taşıma katmanı bağlantı aktörlerinin altında neyi kaybetti? (B58; §6 sonu. Önceden yalnız görev sonu log satırlarındaydı. B66: yazıcının çıkışta yazmadığı kareler — oda/bağlantı onları "gönderildi" saymıştı — ve okuyucunun kapalı kutuya veremediği kare. B73: `udp_frames_drained` yalnız oturumun karelerini sayar, demux'ın ACK taşımasını değil) |
| net | `server_closes` — sebep başına kümülatif (`ServerClose`: `idle_timeout`, `write_stall`, `rel_dead`, `violation_budget`, `preauth_budget`, `stream_rejected`, `conn_cap`, `unauth_cap`, `superseded`, `room_gone`, `outbound_dead`, `idle_input` — E6, odanın girdi-boşta tavanı `afk_action = disconnect` altında; `kicked` — E8, oyunun atma fiili; loadgen telinde GSMG); Prometheus'ta TEK aile `gsb_net_server_closes_total{reason=…}` | sunucu hangi oturumları KENDİ kararıyla, neden bitirdi? İstemci-tarafı son ve shutdown sayılmaz (SECURITY §3.6). Tıkanmış soket ERROR taşıyamadığından istemci sayaçları bunu göremez — `errors=0` bir yük ölçümünde dökülen yarım istemciyi gizleyebiliyordu |

**Adım süresinde iki histogram (ölçüm çözünürlüğü).** `step_hist`
(log-2, bütçe oranları) **bütçe sorusunu** yanıtlar: bütçeye göre
nerede, aşılıyor muyuz (`(1,1)` kenarı = bütçe = aşım sınırı; üst
binler = overflow). Binlerinin 2× ayrık olması bu soru için
tasarımındadır; ama bütçenin çok altındaki bölgede küçük farkları
ayırt edemez (30 Hz bütçede hem 390 µs hem 430 µs aynı binde →
raporda p50 hep "~391" görünür, %10'luk iyileşmeler görünmez). Yanına
`step_fine_hist` eklendi: **sabit 8 µs binler**, `[0, 4096) µs` aralığı
(512 bin) — mutlak µs, bütçe fraksiyonu değil (çözünürlük hedefi adım
sürelerinin gerçekten yaşadığı bölgedeki mutlak fark; tick hızından
bağımsız). Hot path maliyeti: adım başına **bir saturating u32
artış** (tam sayı). Percentiller (`fine_hist_percentile_us`) raporlama
anında saf tamsayı aritmetik: bin alt-kenarı semantiği (hata < 8 µs);
≥ 4096 µs olan adımlar **yalnız** log2 histogram'da → aşım sinyali ve
`over_budget_pct` dokunulmaz. Loadgen satırları: `step_p50_fine_us` /
`step_p90_fine_us` (4096 değeri = "sıra tavan üstünde"; ince bin alt
kenarları 4088'de tavanlanır, bu yüzden belirsizlik yok). Served
stream: **GSM3** (GSM2 + oda başına 512×u32). Gerekçe + ölçülen taban
çalışması: CHANGELOG "Kapatılanlar (ölçüm çözünürlüğü + taban turu)".

**`*_min_us` gerçek minimumdur.** Bir süre öyle değildi: `step_min_us`
ve `late_min_us` iki aktörde de yalnız ilk adımda atanıyordu ve onları
aşağı çeken kol yoktu, yani ilk adımın (tipik olarak en soğuk ve en
yavaş olanın) değerini process ömrü boyunca taşıyorlardı. Kapatıldı:
CHANGELOG "minimum sayaçlar turu". Muhasebenin tamamı — iki extremum,
toplam ve iki histogram — artık `RoomCounters::observe_late_us` /
`observe_step_us`'te, iki aktörün çağırdığı TEK yerde. İlk gözlem iki
ucu da SEED eder (sıfırdan başlayan bir minimum sonsuza dek 0 kalırdı);
`*_min/max/sum_us` üçlüsü kümülatiftir, örnek aralığına ait değil.
Sharded odada rapor katlaması (`loadgen::report::fold_rooms`) bir
minimum için `min`'dir — ve yalnız minimumlar için değil: aşağıdaki
tablo her alanı bağlar.

**Katlama (fold) kuralları — alan başına.** Sharded oda N shard
aktörüdür; her biri kendi örnek kimliğiyle (`room << 16 | index`) ayrı
bir üreticidir, yani toplayıcı N satır raporlar ve tek-oda şeklindeki
her tüketici onları birleştirmek zorundadır. Kural üç turda üç kez tek
tek düzeltildi (`step_min_us`, `late_min_us`, sonra toplu denetim);
sebep alanlar değil ŞEKİL'di — fold ilk satırın kopyasını mutasyona
uğratıyordu, dokunulmayan alan sessizce shard 0'ı raporluyordu. Artık
döngü `RoomReport`'u **tam destructure** eder: yeni bir alan, kuralı
yazılana kadar **derlemeyi kırar** (§13'ün derleme-zamanı koruma
ailesine katılır). Tablo kodda, onu uygulayan tek döngünün yanındadır
(`crates/gsb-server/src/loadgen/report/fold.rs`); burada kaydı için:

| Kural | Alanlar | Neden |
|---|---|---|
| KATLANMAZ | `room` | Ölçüm değil kimlik; katlanmış satır ilk shard'ın id'sini taşır, hiçbir tüketici basmaz. |
| MAX | `steps` | Shard'lar TEK global ticker'la aynı adımda ilerler: odanın adım sayısı bir shard'ınkidir, toplamları değil. `report_steps`'in tazelik ölçütü de budur. |
| MAX | `step_max_us`, `late_max_us`, `snap_bytes_max`, `max_group` | Oda genelinde en kötü hâl (darboğaz shard). |
| MIN | `step_min_us`, `late_min_us` | Oda genelinde en iyi hâl. Bir minimumun foldu `min`'dir, "ilk shard ne dediyse o" değil. |
| MIN (pozitifler) | `hz` | Shard'lar birlikte adımlar, geride kalan shard odayı geri çeker. `hz = 0.0` "bu pencerede örnek yok" demektir (§ `RoomReport::hz`), "durdu" değil — sıfır min'e katılmaz, atlanır. |
| MIN (konfig) | `budget_us` | ÖLÇÜM DEĞİL KONFİGÜRASYON: shard'lar tek `RoomConfig` paylaşır, hep aynıdır. Ayrışırlarsa dürüst cevap küçüktür — aşım oranının ve histogram kenarlarının paydasıdır, küçük bütçe aşımı daha ERKEN okur. |
| ORTALAMA, adım-ağırlıklı | `step_mean_us`, `late_mean_us` | Ortalamaların ortalaması ortalama değildir. Her shard'ın ortalaması `sum / steps` olduğundan `steps` ile ağırlıklandırıp toplam adıma bölmek `Σsum / Σsteps`'i birebir kurar. |
| SUM, eleman bazında | `step_hist`, `step_fine_hist` | Shard dağılımlarının birleşimi; böylece percentiller ve bütçe aşım %'si oda geneli olur. |
| SUM | `lagged_*`, `dropped`, `sends_closed`, `keepalive_resends`, `snapshots`, `snap_overflows`, `snap_records`, `shipped_bytes`, `shipped_frames`, `private_frames`, `joins`, `leaves`, `resumes`, `resume_rejected_stale`, `detach_expired_*`, `detach_forced`, `effects_*` ve `migrations_*` aileleri (küçük paket), `team_*` ailesi (W2), `requests_*` ailesinin tamamı, `metrics_dropped` | Ayrık iş üzerindeki kümülatif sayaçlar. (Bir göç kaynağında `migrations_out`, hedefinde `migrations_in` olarak bir kez sayılır: katlanmış ikili eşit çıkmalı, birbirine eklenmez.) |
| SUM | `dropped_s`, `snap_bytes_s`, `shipped_s` | Shard başına hesaplanmış bir ORAN ortalanamaz: sayaçlar aynı duvar saati üzerinde ayrıktır, odanın oranı toplamlarıdır (ortalamak 4 shard'lık odada kaybın dörtte birini raporlardı). |
| SUM | `groups`, `members`, `detached`, `pending_requests` | Gauge, ama **bölünmüş** gauge — shard'lar odanın bağlantılarını, gruplarını, park edilmiş oturumlarını ve uçuştaki isteklerini PAYLAŞTIRIR, yani odanın değeri toplamdır. Karşı örnek `max_group`/`snap_bytes_max`: bunlar bir popülasyon değil, popülasyon ÜZERİNDE bir uçtur. Toplam, satırlar tek bir an ise bir andır — aşağıda "tutarlı kesit" (F18). |
| SAYAÇ BAŞINA | `logic` | Mantığın kendi sayaçları (F9, aşağıda) kurallarını yanlarında taşır: ad ad, `LogicFold::Sum` toplanır (yukarıdaki SUM satırı gibi ayrık iş), `LogicFold::Max` büyüğü alır (yüksek-su işareti, MAX satırı gibi); yalnız bazı shard'ların bildirdiği ad korunur; taşma sayıları toplanır (`LogicCounters::merge`). |

**Bölünmüş gauge'un toplamı yalnız TUTARLI KESİTTE bir andır (F18).**
Bir rapor, toplayıcının her üreticiden aldığı SON örnektir; shard
aktörleri örneklerini birbirinden bağımsız gönderir (her biri kendi
adımının sonunda, `metrics_every` adımda bir) ve toplayıcı kendi
tick'inde yayar. Rapor bu yüzden aynı turun örnekleri ARASINA düşebilir:
bazı satırlar `k` turu, diğerleri hâlâ `k − 1` (toplayıcı bunu artık
bekleyerek önler — aşağıda "Toplayıcı uçuştaki turu bekler (F29)"). Her satır kendi shard'ı
için kendi örnek tick'inde kesindir; toplamları ise odanın HİÇBİR
anı değildir — iki tur arasında eski-turlu bir shard'dan yeni-turlu
birine göçen oyuncu iki satırda birden görünür (oda bir fazla okur),
ters yönde göçen hiçbirinde görünmez. Çekirdeğin sözü tick indisi
başınadır (CROSS-SHARD §4d): göçen oyuncu kaynağın `members`'ından
kesinleşme tick'i `h`'de çıkar, hedefinkine kurulumda — en erken
`h + 1` — girer; aynı tick'te (ya da bir tick arayla) alınmış iki örnek
onu İKİ KEZ sayamaz, bedeli uçuştaki oyuncunun hiçbir satırda
olmamasıdır (tek tick eksik sayım). İki ya da daha fazla tick arayla
alınmış satırlar iki kez sayabilir: buna **yırtık rapor** denir.
Kesin-bir-kez (uçuştaki dahil) shard'lar arası eşgüdüm ister ve motor
dağıtık kilidi yasaklar; yani sözleşme budur, düzeltilecek bir sayaç
değil. Tüketicinin kuralı: bir raporun satırları ancak **tutarlı
kesit**se (her satır aynı `(steps, lagged_ticks)`'te — shard'lar tek
ticker'la aynı adımda ilerler ve aynı adım katlarında örnekler, yani
eşit çift aynı tur, aynı tick; iki shard'ın ticker aboneliğinin
farkı olabilecek tek tick'i kurulum kapısı soğurur) tek nüfus olarak
toplanır. Tek oda raporu tek satırdır: her zaman kesittir. Kesit
odanın **her shard'ının** satırını içerir (B45): toplayıcı her shard ilk
örneğini göndermeden rapor yayabilir; eksik satırlı bir raporun
satırları `(steps, lagged_ticks)`'te anlaşsa da oda değildir — eksik
shard'ın oyuncuları hiçbir satırda yok. Beklenen shard sayısı koşunun
kendisidir (RESULT'un `shards=`'ı; `print_report` onu
`peak_population`/`steady_span`/`steady_end`/`has_populated_cut`'a
verir). Ve **boş odanın kesiti nüfus değildir** (B52): ilk girişten
önce ve son ayrılıştan sonra kimse göçmez, shard'lar kolayca hizalanır;
oyuncular içerideyken ise her rapor yırtık olabiliyordu — toplayıcı,
shard'ların örneklediği ticker'ın aynısında ve aynı saniyelik periyotla
yayar, yükte yayını onların turunu bölüyordu (F29 bunu toplayıcıda
kapattı, aşağıda; kural, kesiti olmayan koşular için durur). Tek kesitleri kimseyi
tutmayan bir koşunun nüfusunun kesiti yoktur: kesitsiz koşu gibi yırtık
geri düşüşle okunur (ve insan-okunur blok `(torn: …)` der). Eskiden bu
boş kesitler koşunun tek nüfusuydu: yükte `loadgen_orchestrates_the_mmo`
dört botla `shard_members=0,0,0,0`, `records_per_tick=0.0`,
`overlap_x=0.00` basıyordu (orkestre sunucu çocuğu istemcilerden 3 sn
fazla koşar, ayrılış sonrası boş kesitleri hep vardır). Eskiden
kararlı penceredeki raporların hepsi yırtıksa nüfus bu eksik rapordan
okunuyor (4 shard'lık odada 3 değerli `shard_members=`) ve yırtık
satırlara geri düşüş devreye girmiyordu. Loadgen'in
`peak_members`'ı, kararlı penceresi ve `shard_members=`'ı böyle okunur
(`loadgen::report::spread`); pencerenin SONU da (`steady_end`:
`records_per_tick`'in ve savaşın takım penceresinin bitişi, B46) —
eskiden `members == peak_members` olan HERHANGİ bir raporun en
yenisiydi, yani toplamı tesadüfen tepeye eşit yırtık ya da eksik bir
rapor (çift sayım + uçuştaki oyuncu) pencereyi bitirebiliyor ve
`snap_records` farkı farklı adımlardaki shard'lardan alınıyordu; hiç tutarlı kesiti olmayan bir koşu
(eşit olmayan `Lagged` yemiş shard bir daha hizalanmaz) yırtık
satırlardan okunur ve insan-okunur blok bunu söyler. Prometheus
satırları katlamaz (her shard ayrı seri); PromQL'de shard'lar üzerinde
`sum` aynı yırtılmaya açıktır — göç sürerken nüfusun ±1 oynaması
ölçüm değil kesittir. Kilit: `shard::tests::metrics::members` (tick
başına el değiştirme) ve `loadgen::report::spread::tests` (gerçek bir
başarısız koşunun rapor akışı, satır satır; eksik satırlı rapor, B45;
tepeye eşit yırtık/eksik raporun pencere sonu olamaması, B46; yalnız
boş kesitli koşunun rapor akışı, B52).

**Toplayıcı uçuştaki turu bekler (F29).** Yırtılmanın kaynağı
toplayıcının kendisiydi: shard'larla aynı ticker'a abone, raporu
düştüğü tick'te boşaltıp yayıyordu; shard'lar aynı tick'te, her biri
kendi görevinde örnekliyor. Rapor shard'ların örnek tick'ine denk
gelince boşaltma turun ortasına düşüyordu. Ölçüm (F29; `taskset -c 0,1`
+ `yes` yükü ve yüksüz, 4 shard'lı MMO): orkestre koşuda oyunculu
raporların %2,8'i (5/181), ayrık sunucuda %1'i (2/211) yırtıktı;
sunucu çocuğunun odası toplayıcıyla aynı anda kurulunca ilk raporların
fazı turla çakışıyor (B52'nin kısa koşuları), shard'lar hızının altına
düşünce fazları raporunkinin üstünden kayıyor. Şimdi düşen rapor, bir
sharded odanın CANLI satırları `lagged_ticks`'te anlaşıp `steps`'te
ayrışıyorsa (uçuşta bir tur: kimi shard `k`'yı gönderdi, kimi aynı
tick'i hâlâ adımlıyor) bekler; boşaltma sonraki her tick'te yinelenir,
rapor satırları hizalı bulan ilk tick'te çıkar — normalde bir sonraki
tick. Bekleme düştüğü andan `metrics::CUT_GRACE` (250 ms; periyodun
yarısıyla sınırlı, `MetricsCollector::with_cut_grace`) ile sınırlıdır:
ölmüş ya da takılmış bir shard raporları tutamaz; sınırda rapor eskisi
gibi yırtık çıkar (tüketici onu her zamanki gibi `(steps,
lagged_ticks)` ayrılığından tanır; `debug` satırı). Sonraki rapor,
çıkanın bir periyot sonrasına düşer: bekleme yayını turun hemen
arkasına taşır, sonrakiler beklemeden kesittir. Beklemenin
iyileştiremeyeceği ayrılık beklenmez: `lagged_ticks`'te ayrışan
satırlar (eşit olmayan `Lagged`) bir daha hizalanmaz (F20), tek oda
tek satırdır, duran odanın / ölü shard'ın kalan satırları
(`RoomFinal`, `RoomEndedUncounted` — her shard kendi adımında donar)
sorulmaz. Shard satırının odası kimlikten okunur (`room << 16 |
index`'in tersi; `1 << 16` altı tek odadır). Sonra: aynı yükte yırtık
oranı orkestre koşuda 0/182, ayrık sunucuda 0/92. Bedeli: `/metrics`
anlık görüntüsü ve log satırları, uçuşta tur varken periyot sınırından
bir tick (en çok `CUT_GRACE`) sonra yenilenir; raporun biçimi, metrik
adları, tel baytları değişmedi. Toplayıcının takvimi artık tick
saatinde (`ticker::now`: üretimde duvar saati, duraklatılmış testte
sanal saat — kilit testler `metrics::tests::cut`, paused saat). Eksik
satır (henüz ilk örneğini göndermemiş shard, B45) beklenmez: toplayıcı
odanın shard sayısını bilmez.

**Sınırda yırtık çıkan rapor sayılır (F70).** Önceden yalnız `debug`
satırıydı. Şimdi `CUT_GRACE` dolduğunda uçuşta tur varken çıkan her
periyodik rapor toplayıcının kendi sayacına katılır:
`MetricReport::reports_torn_at_cut_grace` (kümülatif; yırtık rapor kendi
sayımını taşır). Kapsam toplayıcının kendisi — üst düzey, kanalın
sağlığı `metrics_dropped`'ın yanında: `gsb-metric scope=net` satırında
`metrics_dropped=`'den sonra `reports_torn_at_cut_grace=`, Prometheus'ta
`gsb_metrics_dropped_total`'dan sonra
`gsb_metrics_reports_torn_at_cut_grace_total`, OTLP'de aynısı; loadgen
telinde üst düzey `metrics_dropped`'tan sonra bir `u64` (tel düzeni
değişti: **GSNL**). Kayıp değil — rapor
çıktı, beklemeden önceki gibi —, sınırın ne sıklıkla aşıldığının
ölçüsü: sıfırdan farklı ve büyüyorsa bir shard takvimini tutamıyor
(ölü, takılmış ya da aç). `with_cut_grace(ZERO)` hiç beklemez: o
yapılandırmada uçuşta turla çıkan her rapor sayılır. `lagged_ticks`'te
ayrışan satırlar (F20) ve son rapor sayılmaz — beklenmezler, sınıra
varmazlar. Kilit: `metrics::tests::cut` (paused saat; hiç göndermeyen
shard → sınırda 1, hizalı rapor 0, sonraki raporlar 1'de kalır).

**Katlanmış histogramın nüfusu `steps` DEĞİLDİR.** `steps` MAX ile,
iki histogram SUM ile katlandığı için katlamadan sonra aynı şeyi
saymazlar. Katlanmış bir histogram üzerinde percentil, histogramın
kendi nüfusuna (`Σsteps`) sorulmalıdır —
`loadgen::report::fold::folded_steps`. Log2 histogram bu nüfusun TAM
kendisidir (her adım binlenir, üst bin sınırsız → `Σ step_hist ==
Σ steps`); ince histogram olamaz, çünkü tavan üstü adımlar kasten onda
yoktur — `fine_hist_percentile_us`'in nüfusu parametre almasının sebebi
de budur. Loadgen'in `step_p50_fine_us` / `step_p90_fine_us` satırları
bir süre tick sayısını veriyordu ve 4 shard'lık odada "p50" etiketi
altında kabaca p12.5 basıyordu; kapatıldı: CHANGELOG "metrik fold
denetimi turu".

**`server_closes` bu tabloda yok — `RoomReport`'ta değil, `NetReport`'ta.**
Net kapsam sunucu başına TEKTİR, `fold_rooms`'un katladığı shard
satırlarından biri değildir; yani yapısal destructure ona dokunmaz ve
bir karar da gerektirmez. Kuralı yine de yazılı: bağlantı aktörü
hükmünü yalnız SON örneğinde ve en fazla bir kez taşır
(`ConnSample::server_close`), toplayıcı bağlantılar üzerinden **SUM**
eder (ayrık oturumlar, her biri tek kapanış); sayaç kümülatif ve
monotondur, bu yüzden loadgen rapor serisinden **toplamı en büyük**
olanı alır (kapanış sonrası son rapor yalnız ekleyebilir). Son örnek
kanal kapanış anında DOLUysa artık düşmez (sayım turu 3): durdurma-mesajı
deyimiyle (`channel::post`) doğurulan bir gönderici yuvayı bekler, aktör
beklemez. Önceden hüküm — ve B59'dan beri daha önce düşen örneklerin
deltaları da — düşer ve düşüş hiçbir yerde sayılmazdı (aktör gitmiştir).
Yalnız toplayıcı gitmişse kaybolur — sunucunun duruşunda artık gitmiş
değildir: son rapor her bağlantı aktörünün bitmesini bekler (F35,
aşağıda "Son rapor üreticileri bekler"). Log satırı: `server_closes=<toplam>` + `server_close_<reason>=N`;
loadgen `RESULT`'ı aynı anahtarları taşır, GSM8 sebep başına bir `u64`.

Prometheus yüzeyi ve log renderer **katlamaz**: örnek kimliği başına
bir satır basarlar (shard'lar `room="r<id>"` etiketiyle ayrı seri), yani
oda-geneli toplama tüketicinin (PromQL'in) işidir. Katlayan tek yer
loadgen'in rapor yoludur, ve orada da hot path'te değildir: fold koşu
sonunda bir kez çalışır.

Her **iki** aktör de binler — oda ve shard, aynı `t0.elapsed()`'ten,
`step_hist` ile aynı satırda. Bu bir süre yalnız oda tarafında doğruydu;
shard alanı gönderiyor ama hiç artırmıyordu ve `fine_hist_percentile_us`
boş histogramda `None` döndüğü için sharded odaların
`gsb_room_step_duration_us` p50/p99 satırları **hiç basılmıyordu**
(loadgen'in `unwrap_or(FINE_HIST_CAP_US)` geri düşüşü ise aynı odaları
tavanda gösteriyordu). Kapatıldı ve kilitlendi: CHANGELOG "Kapatılanlar
(park sızıntısı + shard metrik boşluğu turu)".

Hızlar (`hz`, `*_s`) **örnek aralığı** üzerinden hesaplanır: her oda örneği
kendi `emit_at`'ını taşır (oda `Instant::now()`); oran `latest.emit_at −
prev.emit_at` üzerinedir, rapor penceresi üzerinden değil — örnek gönderim
temposu (vars. 1 Hz = rapor süresi) ile rapor temposu faz-kilitli olmadığı
için rapor penceresi oranı 0–2 örneklik pencerelerde sahte hız verir
(ROADMAP A2).

Rapor satırları kararlı `key=value` biçimindedir (`gsb-metric
scope=room id=r1 steps=.. hz=.. step_hist=[..] ..`) — grep/parse'e uygun.
Ortak bilinen sınırlılık: **shutdown sırasında registry'nin kümülatif
sayaçları oda sayılarının gerisinde kalabilir** (registry `Shutdown`
işlenince break eder; oda, kontrol drenajını — kalan leave'leri — tamamlayıp
çıkar). Kapanış anındaki kesin değerler için oda kapsamı otoritedir.

**Duran odanın son sayımı (B62).** Oda (ya da shard) `Shutdown`'la
(yok etme) ya da ticker kapanışıyla (sunucu durması) durunca, B62'ye
dek başka örnek göndermiyordu: oturumlarının elinde kalanlar —
action kanallarında okunmamış girdi (park edilmiş satırın ölü kanalı
dahil), borçlu kalınan yanıtlar, worker'daki uçuştaki istekler — hiç
sayılmıyordu; son periyodik örnekten sonra sayılan her şey de (bir
rapor periyoduna kadar) kayboluyordu. Artık duruş (`room::stop`,
`finish`: `on_shutdown` → maç sonucu → sayım → son örnek) her oturumu,
SAYIM açısından bir ayrılışın bitirdiği gibi bitirir — mantık kancası
koşmaz: `requests_dropped_unread`/`actions_dropped_unread`,
`requests_abandoned`, `requests_undelivered` (aynı sayaçlar; anlamları
"oturum bitti", odanın duruşu da bir oturum sonu; iki HELP bunu artık
söylüyor) — ve toplayıcıya kendi olayıyla SON örneği verir:
`MetricsEvent::RoomFinal(RoomSample)`. B36'nın elediği "dururken son
örnek"in iki itirazı böyle karşılanır: (1) *diriltme* — toplayıcı
`RoomFinal`'ı yok edilmiş odanın bekleme penceresinde de satıra alır
(periyodik geç örneği orada reddetmeye devam eder), `RoomGone`
gelmemişse (dolu kanalda düştüyse) bekleme penceresini KENDİSİ başlatır:
son örnek "oda gitti" demektir, satır pencereler bitince düşer, hayalet
kalmaz; var olan geri sayım korunur, satır en az bir rapor daha
görünür. (2) *toplayıcının sonuyla yarış* — olay durdurma-mesajı
deyimiyle gider (`channel::post`: yerinde, kanal doluysa doğurulan bir
göndericiden; asla beklemez, dolu kanalda asla düşmez); yalnız toplayıcı
GİTMİŞSE kaybolur. *(B62'de bu "süreç inerken" oluyordu — toplayıcı ve
odalar aynı ticker kapanışında bitiyordu ve son rapor odaların son
örneklerinden ÖNCE çıkabiliyordu; F35'ten beri son rapor onları bekler,
aşağıda.)*
`MatchResultDropped` da artık aynı deyimle gider (önceden dolu kanalda
düşüyordu). Yan etki (turda bulundu): shard satırlarının örnek kimliği
`room << 16 | index`, registry'nin `RoomGone`'u ise mantıksal oda
kimliğini taşır — yok edilen sharded odanın shard satırları hiç
budanmıyordu (hayalet); artık her shard'ın `RoomFinal`'ı kendi satırının
beklemesini başlatır. Kilit: `room::tests::unread::stop`,
`shard::tests::unread::stop`, `metrics::tests::room_final`,
`registry::result::tests`.

**Son rapor üreticileri bekler (F35).** Ticker kapanışı sunucunun duruş
işaretidir ve odalar ile bağlantılar sonlarına ANCAK o an başlar: oda
aynı kapanışı (ya da kontrol `Shutdown`'ını) görür, `on_shutdown` +
maç sonucu + sayımı koşar, `RoomFinal`'ı verir; bağlantı aktörü
registry'nin söküşünden `Shutdown` alır, son flush'ını verir. Toplayıcı
son raporunu kapanışın kendisinde basıyordu — hepsiyle yarış: ilk
periyodik örneğine (metrik periyodu başına bir, sunulan odalarda 30
adımda bir) varmamış bir oda rapordan BÜTÜNÜYLE eksik kalıyordu, her oda
son örneğinden beri saydığını kaybediyordu. Belirti: iş parçacığı
düzeyinde aç bırakmada (≈ 2 Hz oda) süreç içi loadgen raporunda oda
satırı yok (`room_resumes=0`, "server metrics: unavailable";
`loadgen_churn_smoke`), %90 donmada oda satırı yok (F33). Kanıt:
`loadgen_churn_smoke`'un komutu, sürecin bütün iş parçacıkları iki
çekirdeğe itilip nice 19'la 24 `yes`'in arasında: eski ikiliyle 40
koşunun 20'si `steps=0 room_resumes=0` + "server metrics: unavailable";
düzeltmeyle 0/40 (oda satırı `steps=17–39`; 40 koşunun 39'unda oda
ilk periyodik örneğine hiç varmadı, satır yalnız `RoomFinal`'dan). Aynı
kök F33'ü de kapatır: `loadgen_smoke`'un komutu %90 süreç dondurmada
(900 ms SIGSTOP / 100 ms SIGCONT, F25 yöntemi) eski ikiliyle 8/8 oda
satırsız (`steps=0`), düzeltmeyle 0/8 (`steps=12–15`).

Düzeltme toplayıcıda (`metrics/collector/closing.rs`): kapanıştan sonra
elinde kalan tek kaynağı — olay kanalını — bekler, her olayı katlar ve
kanal KAPANINCA (her gönderici düştü = her üretici bitti) son raporu
basar. Üreticinin son sözü bitmeden gönderilir; dolu kanalda
doğurulan göndericiden giden (`channel::post`) kendi gönderici klonunu
teslim edene dek tutar — kapanış ikisini de geçemez (işaret olayı ve
bariyerle yapılan sıralamada bu yarış kalırdı). Bekleme kapanıştan
itibaren `FINAL_REPORT_GRACE` (2 sn) ile sınırlı: duruşu aşan bir
üretici (takılı oda; F41'den önce registry'nin söküşünden sonra teslim
edilmiş bir bağlantı da) son raporu tutamaz; rapor sınırda onun son sözü olmadan çıkar,
`warn` + `run()` `false` döner → `StopReport::final_report_complete`.
Sınır `stop()`'un kendi sınırlarını (accept 1 sn + odalar 1 sn +
servisler 1 sn) uzatmaz: onlarla yan yana koşar. F41'den beri ticker
accept döngüleri bittikten sonra kesilir, sınır da o andan başlar: accept
hattı (her yeni bağlantı aktörüne göndericisini veren) kapanıştan önce
bitmiştir.

**İki kanal.** Oturum üreticileri (registry, oda/shard, ölüm bekçisi,
dağıtıcı, bağlantı aktörü, accept hattı) ana kanala; taşıma görevleri
(kapıların pump'ları, rUDP demux/yazıcı, el sıkışma kabulü — `gsb_net`
`TransportMetrics`) ayrı bir kanala (`with_transport_events`, aynı
4096) gönderir. İkincisi aynı şekilde katlanır (her tick'te ve bekleme
sırasında her olaydan sonra, en sonda bir kez) ama kapanışı BEKLENMEZ:
okuyucu pump'ı soketiyle biter ve sessiz bir eş (ERROR 14'ten sonra
soketi kapatmayan, yarı açık) onu duruşun ötesinde tutabilir — tek
kanalda her böyle duruş sınırı bekler, raporun gecikmesi eşe bağlı
olurdu. Taşımanın duruştaki son flush'ları eskisinden kötü değil:
son rapor artık oturumların hepsinden sonra çıkar (önce kapanışta).

*Elenenler:* (1) `stop()`'ta ticker'ı odaların bariyerinden (F5) sonra
kesmek — odalar kontrol `Shutdown`'ını bir sonraki tick'te işleyip
biterdi, ama bağlantıların son flush'larını kapsamaz, dolu kanalda
doğurulan göndericinin sırasını vermez, duruşun sırasını değiştirir;
(2) `stop()`'un bariyerden sonra
kanala bir "son" işareti göndermesi — dolu kanalda doğurulan bir
göndericinin `RoomFinal`'ı işareti geçemeyebilir (sıra zamanlayıcıya
kalır), bağlantılar için ayrı bir bariyer gerekir; kanal kapanışı ikisini
de yapıdan verir; (3) tek kanal + sınır — sessiz eşli her duruş sınırı
bekler; (4) loadgen'in kör beklemesini uzatmak — aç oda periyoda hiç
varmayabilir. Loadgen'in `final_sample_grace`'i (150 ms + bir metrik
periyodu, B36) 150 ms'lik ayrılış oturmasına (`LEAVE_SETTLE`) indi:
odanın sayıları duruşun son raporundan gelir; ilk periyodik örneğine
varmamış oda da raporda.

Kilitler: `metrics::tests::final_report` (paused saat: kapanıştan sonra
gelen `RoomFinal` + bağlantı son flush'ı son raporda; ölmeyen üretici
raporu tam sınırda bırakır, `false`; taşıma kanalı beklenmez),
`service_stop::the_final_report_carries_every_rooms_final_count` (ilk
örneğinden önce durdurulan ve `on_shutdown`'da 200 ms bekleyen iki oda
son raporda; eski davranışla 5/5 kırmızı),
`loadgen_smoke::a_run_shorter_than_a_metrics_period_still_reports_the_room`,
`accept_stop` (`final_report_complete: true`).

**Duran odanın op'ları ve shard'ın uçuştaki işi (B68, sayım turu 4).**
B62 duruşta OTURUMLARIN elindekini saydı; geri kalanı sayılmıyordu:
kontrol kanalında (shard'da gelen kutusunda) `Shutdown`'ın arkasında
kalan — ya da ticker kapandığında orada duran — bağlantı op'ları ve
shard'ın komşularla uçuştaki işi. Artık `finish` kanalı KAPATIR
(sonraki gönderim göndericide başarısız olur ve ret bir şey
kaybettiriyorsa orada sayılır — katılma B75, göç `migrations_failed`;
ayrılma/taşıma ölümü sayılmaz, F55 aşağıda; sayım kesindir) ve kalanları
sayar, `metrics::StopCounts` (dokuz sayaç,
`RoomSample::stop`/`RoomReport::stop`; yalnız son örnekte dolu): `Join`
→ `joins_unprocessed` (bağlantının katılmasına dağıtıcı `RoomGone`
yanıtı verir — cevapsız kalmaz, ama oda onu hiç işlemedi), `Resume` →
`resumes_unprocessed`, `Leave`/`Detach` → `leaves_unprocessed` /
`detaches_unprocessed` YALNIZ burada etki edecekse (bayat-ayrılma
korumaları; bayat op kayıp değildir; `Detach`'te `on_disconnect` hiç
koşmadı). Shard'da yayın op'ları (`Leave`, `Detach`, `Resume` her
shard'a gider) yalnız etki edeceği shard'da sayılır (üyenin sahibi,
kimliği park etmiş olan) — shard sayısı kadar şişmez; kimsenin parkında
olmayan bir resume ise dağıtıcıda taze katılmaya düşer ve orada bir kez
sayılır (B75: ev shard'ı kapalıysa dağıtıcının `joins_refused_closed`'ı,
açıksa ev shard'ının `joins_unprocessed`'i); parkı duran shard'da olan
resume'a o shard `RoomGone` der ki taze katılma onu ikinci kez saymasın.
Shard'a özgü: `migrations_in_dropped`,
`effects_unsent`, `effects_unapplied`, `team_imports_unapplied`,
`border_updates_unapplied` (CROSS-SHARD §4b "Duran shard'ın elinde
kalanlar"). Yan bulgu (düzeltildi): shard'ın CONTROL fazı `Shutdown`'a
rastlayınca aynı boşaltmanın kalanını vec'le birlikte sayılmadan
atıyordu; artık ertelenmiş kuyrukta sayıma kalır. Anlam notu: bir
göçte gönderici `migrations_out` sayar, alıcı durmuşsa ya
`migrations_in_dropped` (kutuya girmişti) ya göndericide
`migrations_failed` (kutu kapanmıştı) — her göç tam bir yerde. Kilit:
`room::tests::unread::stop`, `shard::tests::unread::stop`, loadgen
`wire`.

**Duruşun ayrılma/taşıma-ölümü defteri (F55, sayım turu 8).** Dağıtıcının
odaya ayrılması (`send_room_leave`) ve taşıma ölümü (`send_room_detach`;
registry'nin dağıtıcısız iki gönderimi de) oda/shard kutusunu `finish`
kapattıktan SONRA varırsa reddedilir ve hiçbir yerde sayılmaz; ÖNCE
varırsa `leaves_unprocessed`/`detaches_unprocessed` olur. Hangisinin
olacağı zamanlamaya bağlı. **Karar: sayılmıyor; B68'in iki sayacının
anlamı daraltıldı.** Gerekçe: (1) Kayıp yok. Kutu yalnız `finish`'te (ya
da görevin ölümüyle, B67) kapanır; `finish` o an tuttuğu HER üyeyi
bitirmiştir — `on_shutdown` ve maç sonucu koştu, oturumların elindeki
B62 ile sayıldı (işlenmiş bir ayrılmanın oturum sonunda sayacağı
okunmamış girdi, borçlu yanıt, uçuştaki istek aynı sayaçlarda). Sonra
varan op'un etki edeceği üye kalmadı: B68'in kendi ölçütüyle (odanın
canlı-üye korumaları) bayattır ve bayat op kayıp değildir. Kimse farkı
görmez: istemci (taşıması ölü, ya da duruşun `ERROR 14`'ünü alıyor;
LEAVE'in cevabını bağlantı zaten vermiştir), registry (dağıtıcı
`LeaveDone`/`DetachDone`'u yine gönderir), oyun (dünyası bitti; duruşun
bitirdiği hiçbir üyede `on_disconnect`/`on_leave` koşmaz). (2) Gönderici
tek anlamlı sayamaz. Dağıtıcı üyeliği bağlantı kapanana dek tutar: yok
edilen ya da ölen odanın üyesi saatler sonra kopunca aynı reddi alır,
oda üyeliği bir hükümle (atma, idle) bitirdiyse de — sayaç "duruşun
yuttuğu" ile "çoktan bitmiş üyelik"i karıştırırdı. Sharded odada yayın
op'u her shard'a gider ve B68 onu yalnız sahip shard'da sayar; dağıtıcı
reddeden shard'ın sahip olup olmadığını bilmez — "herhangi biri
reddetti" çift sayar, "hepsi reddetti" kaçırır. Kesin sayım üyelik başına
odanın çözdüğü bir işaret isterdi (her Join/Resume yanıtına, satıra, göç
yüküne) — kayıp olmayan bir şey için. **Daralan anlam:**
`leaves_unprocessed`/`detaches_unprocessed` duruşun DEFTERİdir — oda/shard
kanalına ALDIĞI ve üye hâlâ oradayken işlemediği op'lar; duruşun
geçersiz kıldığı her ayrılma/taşıma ölümü değil. HELP metinleri de öyle
der. Aynı gerekçe F53'ün kararıyla tutarlı: registry kutusunda
`Shutdown`'ın arkasında kalan `DespawnPlayer`/`ConnClosed` da sayılmıyor.
Kilit (iki varış sırası da deterministik):
`registry::actor::conns::ops::tests::superseded` (oda aldı → sayaç
yok, odanın duruşu sayar; oda durmuştu → ret, dağıtıcı yine
`DetachDone`/`LeaveDone` + `OpsClosed`, sayaç yok; bir shard açık biri
kapalı yayın) ve
`room::tests::unread::stop::a_live_members_detach_is_the_stops_count_only_if_it_was_taken`
(alınmış → `detaches_unprocessed = 1`, reddedilmiş → 0; üyenin sonu iki
sırada da B62 ile sayılır).

**Panikle ölen oda/shard (B67, sayım turu 4).** Panikleyen görev
`finish`'e hiç varmaz: son örnek gitmez, son penceresi ve elinde
kalanlar (okunmamış girdi, uçuştaki istekler, borçlu yanıtlar, kuyruktaki
op'lar) bilinemez — durum panikle birlikte gitti. Ölen shard'ın satırı da
hiç budanmıyordu (`RoomGone` mantıksal oda kimliğini taşır). Görevin
bittiğini gören tek yer ölüm bekçisidir (`registry/actor/rooms/watch.rs`):
`JoinHandle` bir hata döndürdüyse (panik ya da iptal) görevin SATIR
kimliğiyle (oda kimliği ya da `shard::sample_id` = `room << 16 | index`)
`MetricsEvent::RoomEndedUncounted` gönderir (durdurma-mesajı deyimi,
`channel::post`). Toplayıcı onu sayar (registry dilimi
`rooms_ended_uncounted`, `gsb_registry_rooms_ended_uncounted_total`) ve
satırın beklemesini son örnek gibi başlatır: satır son periyodik örneğiyle
iki pencere görünür, geç örnekleri reddeder, sonra düşer — hayalet yok.
**Sayılabilen bu kadar:** görevin kendisi sayılır; kaybolan pencerenin ve
elde kalanların SAYILARI bilinemez (panik sonrası kancadan sayım,
`Drop` koruyucusu içinde `spawn` — ikinci panik süreci düşürür — elendi).
Anlam notu: `rooms_died` registry'nin biçtiği MANTIKSAL odaları sayar,
`rooms_ended_uncounted` son sayımsız biten GÖREVLERİ (her shard kendini);
sonradan `Drop`'ta panikleyen bir görev son örneğini göndermiş olsa da
sayılır (sayısı tam, sayaç bir fazla — kabul edilen uç). **Yan bulgu
(düzeltildi):** bir shard paniklediğinde registry mantıksal odayı biçiyor
ama HAYATTA kalan shard'lara `Shutdown` göndermiyordu — birbirlerinin
link'leri gelen kutularını açık tuttuğundan sunucu durana dek tick
atıyor, `RoomGone` almış üyelere yayın yapıyor, var olmayan bir odanın
satırlarını raporluyorlardı. `on_room_died` artık kaydı `stop_room` ile
durdurur (yok etme gibi); hayatta kalanlar kendi `RoomFinal`'larıyla
biter. Kilit: `tests/supervision/uncounted.rs`,
`metrics::tests::room_final`.

**Her sayacın bir doğru-yol testi vardır.** Bu yüzeyin sayaçları
(`RoomSample`, `RegistrySample`, `ConnSample`, `UdpClientStats` — ve
`RoomReport` üzerinden rapora ulaşan her alan) tek tek, gerçek üretim
yolu sürülerek ve O alanın arttığı assert edilerek kilitlenmiştir;
kapsama tablosu ve kalan kuyruk: CHANGELOG "sayaç envanteri kapanış
turu". Yeni bir sayaç eklerken kural aynıdır ve iki parçalıdır:
(1) elle kurulmuş bir örnek üzerinde assert etmek SAYMAZ — aktörü sür;
(2) alanın yalnızca SIFIR olduğunu assert eden bir test, hiç yazılmayan
bir alandan ayırt edilemez (bu depoda iki kez gerçekten olan hata: hep
sıfır kalan bir sayaç ve hiç yazılmayan bir histogram) — sayacı artıran
yolu tetikle, ve testi onu en çok benzediği KOMŞUSUNDAN ayıracak
biçimde yaz (tepe ≠ sonuncu, akış ≠ gauge, ölüm ≠ destroy, ihlal ≠
trafik, kadans ≠ kayıp). Testi mutation-check et: artışı boz, testin
düştüğünü gör, geri al.

**Mantığın kendi sayaçları (F9).** `RoomSample` sabit biçimli bir
yapıdır ve her yeni çekirdek sayacı ~10–29 dosyaya dokunur; çekirdeğin
bilmediği olayların (kit'in crystallization olayları, oyunun
öldürmeleri) hiç yolu yoktu. Seam: `GameLogic::logic_counters(&self,
world, out: &mut LogicCounters)` (varsayılan boş). Sayaç bir `const`
bildirimdir (`LogicCounter::sum/max(ad, help)` — ad kuralı const
değerlendirmede denetlenir; çalışma zamanında kayıt yok), değer
mantığın kendi alanındadır (tick içinde `self.kills += 1`: tahsis yok,
kilit yok, mesaj yok) ve aktör onu **örnek başına bir kez** (rapor
temposunda, adım sonrası) sabit boyutlu `LogicCounters`'a okutur; küme
`RoomSample`/`RoomReport` içinde değerle taşınır (`Copy`, en çok
`LOGIC_COUNTERS_MAX` = 16 yuva; ad satır içi 32 bayt, böylece telden
çözülen sayaç da aynı `Copy` değer). Sınırı aşan ad atılır, kümede
sayılır, aktör bir kez `warn` eder (adlar oyun başına statik: sınır ilk
test koşusunda görülür, yük altında ortaya çıkmaz). Aynı ad bir kümede
iki kez konursa kuralıyla katlanır — shard katlamasının aynısı. Katlama
kuralı iki tanedir: SUM (ayrık iş, Prometheus `counter`) ve MAX
(yüksek-su işareti, `gauge`); ortalama/min gerekmedi (kümülatif bir
sayacın başka anlamlı katlaması yok, gerekirse yeni bir `LogicFold`
kolu). Toplayıcı ve Prometheus/log renderer yine katlamaz; görünüm
biçimleri ve elenen `name` etiketli aile: OPS §3. Varsayılan
değişmedi: sayaç bildirmeyen mantığın metni önceki kodun ürettiği
metne bayt bayt sabittir (`metrics::tests::golden`).

**Sınır taşması görünür (BACKLOG F17).** 16'yı aşan adlar düşürülüp
yalnız kümenin içinde sayılıyor ve aktör bir kez `warn` ediyordu: satırda
ve Prometheus'ta görünmüyordu (loadgen teli sayıyı GSMC'den beri
taşıyordu, gösteren yoktu). Artık sayı çekirdeğin kendi anahtarıdır:
satırda mantığın anahtarlarından sonra `logic_counters_dropped=<n>`,
Prometheus'ta `gsb_room_logic_counters_dropped{room=…}` (`gauge`),
loadgen `RESULT`'ında `logic_*` anahtarlarından sonra aynı anahtar.
Üçü de **yalnız sıfırdan büyükken** basılır: sınır içindeki bir
mantığın (yani bugünkü her mantığın) metni bayt bayt aynı kalır
(`metrics::tests::golden` değişmeden geçer). `gauge`: değer, odanın SON
örneğinde sığmayan ad sayısıdır — statik bir bildirim hatası her örnekte
aynı sayıyı verir, birikmez; `counter` her raporda bir artan anlamsız
bir oran üretirdi. Anahtar mantığın ad alanında (`logic_` öneki)
durduğundan `counters_dropped` adı **ayrılmıştır**:
`LogicCounter::sum/max("counters_dropped", …)` const değerlendirmede
derleme hatası, telden çözülen ad reddedilir — bir oyunun sayacı
çekirdeğin anahtarıyla çakışamaz. Elenenler: (1) *Sıfırken de basmak* —
her odanın satırına ve her kazıya yeni bir anahtar/aile ekler, altın
metni ve "sayaç bildirmeyen mantık aynı metni üretir" sözünü bozar.
(2) *`RoomSample`'a ayrı bir alan* — sayı zaten örnekte `LogicCounters`
içinde değerle taşınıyor (ve telde); ikinci bir kopya tutarlılık yükü
olurdu. (3) *`logic_` önekinin dışında bir ad* (ör.
`dropped_logic_counters=`) — ad ayırmayı gerektirmezdi, ama anahtar
satırda ve kazıda mantık sayaçlarından kopardı; tek bir ayrılmış ad
daha ucuz.
Testler: `metrics::tests::logic` (18 adlı oda: satırın sonu
`logic_c15=15 logic_counters_dropped=2`, Prometheus'un sonu tek odalı
gauge ailesi; sınır içindeki odada ikisi de yok),
`room::tests::logic_counters` (aktörün örneği toplayıcıdan geçip satırda
anahtarı taşır), `metrics::logic::tests` (ayrılmış ad reddedilir),
loadgen `report::logic::tests` (RESULT). Mutasyonlar: satır ya da
Prometheus kolunu kaldırmak, ayırmayı kaldırmak, RESULT kolunu kaldırmak
yeni testleri; "yalnız sıfırdan büyükken" şartını kaldırmak altın testi
düşürür.

**Dışa açım katmanı (BACKLOG E2).** Toplama yukarıdaki gibi kalır;
raporun sunucudan çıkışı toplayıcının `emit`'indeki TEK dikiştedir:
`Exporter` trait'i (`fn export(&mut self, &MetricReport)`), kurulu
exporter'lar sırayla ve senkron çağrılır, sonra sink raporu tüketir.
Exporter saf tüketicidir (aktörlere/toplayıcıya uzanan tutamak yok,
aktör kodu değişmedi) ve bloklamaz: G/Ç yapan exporter raporu kendi
görevine sınırlı devirle verir — OTLP'de tek yuvalı kanal +
`try_reserve`, dolu yuvada rapor düşer ve sayılır (kümülatif değerler:
sonraki devir düşeninkini zaten taşır; örnek kanalıyla aynı mantık).
Çekme yönü (Prometheus) exporter değil anlık görüntüdür: ops yüzeyinin
`watch`'ı kazıma anında render edilir. Prometheus ve OTLP **tek aile
tablosunu** (`metrics::export::families`) yürür, iki yüzey ad/tür/değer
olarak birbirinden kopamaz (`metrics::tests::otlp::cross`); Prometheus
metni tablo taşınırken bayt bayt aynı kaldı (`metrics::tests::golden`).
Her exporter bir feature: `gsb-core`'da `prometheus` (varsayılan) ve
`otlp` (kapalı), `gsb-server` ileri taşır; `gsb-core` workspace'e
varsayılan feature'sız bağlanır. OTLP yeni bağımlılık getirmez (elle
`prost` derive'lı mesaj alt kümesi + tokio `TcpStream` üstünde tek
HTTP/1.1 POST). Eşleme tablosu OPS §3'te, kararlar/elenenler OPS §6'da.

**Kullanım:** `gsb-server` çalışırken `RUST_LOG=info` → metrik satırları
logda; `gsb_server::start_server_metrics(cfg, tx)` → raporlar kanaldan
programatik (yük üreticisi ve testler bu yoldan kullanır); `http_listen`
→ `/metrics` (Prometheus); `[metrics.otlp]` (`otlp` feature'ıyla
derlenmiş sunucuda) → her aralıkta bir OpenTelemetry collector'ına
OTLP itmesi. Yük testi sayıları ve ilk doyma analizi: CHANGELOG
"Kapatılanlar (metrik + yük turu)".

## 13. Derleme zamanı korumaları

- `unsafe_code = "forbid"` — her crate'te.
- `gsb-lint` — 6 crate'in `build.rs`'inde (lint crate'i hariç) select/kilit
  desenleri build hatası; kapsam `src/` + `tests/` + `examples/`.
- `cargo clippy --workspace --all-targets` temiz.
- `edition = "2024"` (Rust 1.95).
## 14. Bu base neyi hedefliyor, neyi hedeflemiyor

Bu bölüm, üstündeki maddelerin *neden* böyle olduğu sorusunun tek yeridir.
İlkeleri koyuyor, ölçülen duvarla birlikte sınırları çiziyor.

### 14.1 Değişim birimi snapshot grubudur — bağlantı değil

Oda, başına **durum** tuttuğu tek şey üyelik tablosudur; fan-out maliyeti
bağlantı başına değil **grup başına** kodlanır (snapshot bir kez kodlanır,
`freeze()` ile referansla dağıtılır — §4 BROADCAST). Sonuç:

- 100k bağlantı tek grupta: tick başına 100k Arc klonu + `try_send`, 1
  kodlama. 100k bağlantı 100 gruba yayılmışsa: kodlama aynı, bağlantı
  başına fan-out 1/100.
- Ölçekleme kaldıracı **"bağlantıyı ucuzlatmak" değil, gruplama kalitesidir**
  (AOI / takım / sektör — §8.1). Demo'nun dört görünürlük stratejisi bu
  kaldıracın dört ilacıdır.
- **Bağlantı başına durum yok** (oda tarafında): connection actor kasıtlı
  olarak ince bir durum makinesidir (auth → join → forward); odaya giren
  bağlantının kimliği `Action.conn`'de taşınır, oda onu geri bildirimde
  kullanır. Bu, "oda N bağlantı"yı bellek ve kod karmaşıklığı açısından
  "oda N grup"la sınırlı tutar.

### 14.2 Sıralama ve onay: taşıma ne taşır, oyun katmanı ne taşır

- **Teslim garantisi taşıma katmanının** (TCP; rUDP'de kontrol bandının).
  Oyun katmanı teslim için yeniden gönderim yapmaz; taşıyanı bilmez.
- **Snapshot seq** = oda tick'inin global indeksi (kural: seq = tick).
  Delta modunda akış olay-odaktır (grup yalnızca değişimde yayınlar; wire
  kuantizasyonu — §8.1 "Delta yayın" — boşlukları normal duruma çevirir);
  boşluk kaybın kanıtı değildir, **yakınsama garantisi keepalive full'ıdır**
  (bir periyotta tam durum). "Kayıp paket = bir keepalive periyoduna kadar
  bayatlık" kabulü delta modda da geçerlidir; tam-snapshot stratejilerinde
  (all/team/pvs) davranış değişmedi (kayıp = bir kademe bayatlık, örtülür).
- **Girdi seq/ack (bu tur; yeni):** istemci girdileri oturum başına 1'den
  başlayarak numaralandırır; oda, bağlantı başına **en yüksek su** tutar
  (`InputState{hwm, acked}` — iki u64; §14.1'in "bağlantı başına durum yok"
  ilkesine **bilinçli ve belgelenmiş istisna**: durum bir sıra sayısı +
  bir onay damgasıdır, tampon/zamanlayıcı yok). Kural: `seq > hwm` → işle +
  `hwm = seq`; `seq ≤ hwm` → sessizce at (normal yarış, ihlal değil); boşluk
  işareti engellemez (yüksek-su, ardışıklık değil); `seq = 0` = numaralandırma
  öncesi (işlenir, hwm'yi ilerletmez, asla ack'lenmez — eski istemciler).
  Onay, `processed_up_to` olarak bağlantının **private** frame'inde döner
  (`Private{ack|snapshot}` oneof'u) — sunucu *işlediğinden* fazlasını asla
  ack'lemez (test kilitli: `ack_is_monotonic_and_never_exceeds_processed`).
  Tick başına bağlantı başına en fazla bir private frame: full gelirse ack
  bir tick ertelenir (full kazanır). Rejoin'de iki taraf sıfırdan başlar
  (sunucu `on_join`'da sıfırlar; yeni istemci'nin seq 1'i eski hwm'nin
  altında kalsın diye). Bu bir **işleme işareti**dir — prediction
  mutabakatı için ("girdime kadar ne uygulandı?") — teslim garantisi
  değil; rUDP yolundaki gibi RTO yeniden gönderimi yoktur. Elenmiş
  alternatifler: (1) ardışıklık tabanlı onay (kayıp boşluk beklenir) —
  kayıp-toleranslı bandda işareti kayıba bloke eder, "1 kademe bayatlık"
  kabulüyle çelişir; (2) taşıma katmanında (rUDP kontrol bandı) — kontrol
  bandının işi (AUTH/JOIN/LEAVE) zaten öyle; girdi işareti *oyun*
  anlamındadır ve oyun bandının rejoin/loss toleransı içinde yaşamalıdır;
  (3) seq'siz statüko — dupl girdi entity'yi sessizce geri taşıyabilir,
  rejoin'de mutabakat noktası yok.
- Deterministik lockstep, paket sıralama garantisi, hata düzeltme (FEC):
  hedeflenmez.

### 14.3 Ölçülen tavan tek odadadır — kaldıraç oda segmentasyonu

C1 ölçümü (ayrı proses, spatial AOI, 30 Hz oda, tek oda; bkz. ROADMAP
"Kapatılanlar (koruma katmanı turu)"):

| Oyuncu | p50 (ms) | server_hz | fan-out drop | late (s) |
|---|---|---|---|---|
| 5k | 12.5 | ~30 | 9 086 | ~0 |
| 8k | 25 | ~30 | 45 596 | ~0 |
| 9k | 25 | ~30 | 30 869 | ~0 |
| 10k | **50** | **23.2** | 52 771 | **2.17** |

p50, 33.3 ms bütçesini **9k ile 10k arasında** aşıyor. Duvarın kaynağı
mimari bir bug değil, *tasarımın doğal sonucudur*: oda actor'ü tek
iş parçacıklıdır (tek `World`, tek senkron tick gövdesi, tek await),
dolayısıyla bir oda tek thread'in bütçesiyle sınırlıdır. Sunucunun bütünü
(100k bağlantı, §1 hedefi) bu duvarın **çoklu oda** ile aşılır: oda
segmentasyonu — her oda bütçe içine sığar, bağlantılar odalara dağılır.
Varsayılan `max_players = Some(10_000)` tam olarak bu ölçülen duvardır
(§10); bir oyun bunu kendi bütçesine göre (çok) daha düşüğe çekmelidir.

**Ölçülerek doğrulandı (oda segmentasyonu turu, `sharded`):** aynı 10k
bağlantıda tek oda (`all`) adım bütçesini aşıyorken (`step_max` ~73 ms,
`over_budget` %3), `sharded N=4` bütçeye içe girer (`over_budget` ~0),
`sharded N=8` p50 adımı ~8× düşürür; bedel ~1,4-1,5× sunucu CPU (protokol
trafiki). Yani **segmentasyon adım-duvarını kaydırır**; duvarın *kaynağı*
(tek actor = tek thread bütçesi) aynen kalır, shard'lar arası dağılır.
Bölünebilir oyunlarda çoklu oda aynı işi (daha) ucuz yapar; segmentasyon
özel olarak *tek sürekli dünya* sınıfına aittir. Detay + elenmiş
alternatifler: CHANGELOG "Kapatılanlar (oda segmentasyonu turu)".

### 14.4 Hangi oyun aileleri sığıyor

**Sığıyor** — dünyanın doğal olarak odalara segment edildiği ve oda
bütçesine sığındığı aileler:

- **Battle royale** (≤100 oyuncu/oda): bolca marj; tek oda, tek grup bile
  rahat.
- **Arena / MOBA / partiler** (10–30 oyuncu): marj 2–3 haneli.
- **Zoneli MMO / MMOFPS bölgeleri** (AOI ile binlerce oyuncu): spatial
  stratejiyle grup başına üye sayısı yüzlerce; ölçülen adım maliyeti
  (O(entity + üye), §11 tablosu) ile sınırlı.
- **Instanced dungeon / raid**, **lobili oyun**, sohbet-yanlı MMO alanları.
- **Oda sınırı olmadan büyük ama sınırlı tek dünya** (`sharded`, §8.2):
  tek dünya N shard'a segment edilir, entity'ler sınırda migrasyonla
  taşınır. Ölçülen: 10k bağlantıda adım-duvarı aşılmaz (N=4/8). Sınır:
  shard'lar tek makine içindeki actor'lardır (çok makineye dağıtım
  katmanı eklenmeden); shard sayısı 1..=16.

**Sığmıyor (katman eklenmeden)** — bu base'in kapsamı dışı:

- **100k+ oyunculu sınırız tek dünya** (çok makine): `sharded` tek
  makinede N shard'a kadar gider; çok makine dağıtımı (shard'ların
  network'e yayılması) ayrı bir katmandır ve henüz yapılmadı.
- **Bağlantı başına oturum durumu gerektirenler**: sohbet oturumları,
  istemci başına prediction/buffer, bağlantı başına rate muhasebesi —
  connection actor ince tutulmuştur; bu durumlar *üzerindeki* uygulama
  katmanına aittir (veya §14.2'deki rUDP yoluna).
- **Taşıma düzeyi teslim garantisi / yeniden gönderim veya paket
  sıralama garantisi gerektirenler** (14.2) — girdi düzeyi sıra/ack
  (işleme işareti) bu tur eklendi; oyun bandının teslimi hâlâ
  taşıma katmanındadır.
- **Mikro-saniye determinizmi / lockstep** (tick determinizmi vardır —
  aynı tick'te aynı adım — ama istemci-sunucu clock senkronu yoktur).

### 14.5 Koruma katmanının kapsam notu (bu tur)

Sunucunun, istemcilerinin yaptığı **hiçbir şeyden** çökmeme / kaynak
sızdırmama garantisi üç hatla kuruldu (detay + 100k matematiği: ROADMAP
"Kapatılanlar (koruma katmanı turu)"):

1. **Yaşam döngüsü:** yarım açık TCP, reader pump'un read deadline'ı ile
   yakalanır (varsayılan 30 sn, 0 = kapalı) — bağlantı yolundaki tek saat.
2. **Kapasite:** oda `max_players` (vars. 10k = ölçülen duvar) + sunucu
   geneli `max_connections` (vars. 100k = §1 hedefinin guardrail'i);
   semantiği nazik reddi (ERROR 8 = oda dolu, bağlantı yaşar; ERROR 9 =
   sunucu kapattı, hemen kapatılır).
3. **Adalet:** girdi kaybı yalnızca göndericinin kendi kanalında ve
   atfeli (`actions_dropped_top`); oda çektiği aksiyonu asla atmaz —
   flooding başkasının aksiyonunu evicted edemez.
4. **Anti-amplifikasyon (protokol-ihlal bütçesi):** bağlantı başına
   yerel, ağırlıklı **ömür boyu** bütçe (16 puan: Hard ihlal 4, Race 1,
   sunucu tarafı koşul 0); ilk 3 ihlal cevaplanır (istemci geliştirici
   tanısı), sonra huni susar (cevap amplifikasyonu sınırlı), bütçe
   tükenince ERROR 9 + kapatma; peer adresi sinyalle taşınır (WARN
   satırı firewall/fail2ban'a doğrudan girebilir). Detay: ROADMAP
   "Kapatılanlar (ihlal bütçesi + rUDP turu)".
