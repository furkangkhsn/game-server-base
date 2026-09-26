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
  bloke edemez.
- **Accept loop:** `ConnectionId` üretir, pump görevlerini başlatır,
  `ConnOpened`'ı **actor'ü başlatmadan önce** registry'e gönderir (ilk
  istemci frame'ine karşı sıralama garantisi). Kalıcı hata durumunda
  (ör. `EMFILE`) 100ms backoff ile dener — CPU spin'i olmaz.
- **Metrik toplayıcı:** odaların/registry'nin/bağlantıların sayacalarını
  **kanaldan** toplayan tek görev (bkz. §12). Saat kaynağı ticker'ın
  broadcast'i — odalarla aynı tek-await disipline sahiptir; ticker kapanınca
  son raporu basıp çıkar.

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
  tablosunda değiller); `1000+` oyun bandı
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
  özellik pazarlığı) tartışılır. Önce veri.

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

**Kapanış yolları, kapı kapı (önce → sonra).** Değişmeyenler: `idle_timeout`,
`violation_budget`, `preauth_budget`, `conn_cap`/`unauth_cap`,
`superseded` → ERROR 9 (beklemeli gönderim, stall penceresiyle sınırlı);
`room_gone` → ERROR 5; `write_stall`, `rel_dead`, `outbound_dead` →
bildirim YOK (bildirimi taşıyacak yol ölü — sayacın var olma sebebi).

| Kapı | `stop()` önce | `stop()` sonra | `stream_rejected` önce | `stream_rejected` sonra |
|---|---|---|---|---|
| TCP | sessiz FIN | ERROR 14 → FIN | sessiz FIN | ERROR 9 → FIN |
| TLS | sessiz close_notify/FIN | ERROR 14 → close_notify/FIN | sessiz | büyük kare: ERROR 9 → son; **bozuk kayıt: bildirim YOK** (TLS oturumu ölü, rustls fatal alert'ini göndermiş, yazma başarısız) |
| WS | boş kapanış çerçevesi (istemcide 1005) | ERROR 14 (binary mesaj) → boş kapanış çerçevesi | kapının kendi kapanış çerçevesi (1002/1003/1007/1009) | **aynı** — kapanış çerçevesi BU kapının bildirimi; RFC 6455 §5.5.1 kapanıştan sonra veri çerçevesini yasaklar, soket yazıcı görevi arkasına düşen ERROR 9'u (ve fan-out artığını) atar |
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
   öğrenmek zorunda kalırdı. (WS'in durdurmadaki kapanış çerçevesi boş
   kaldı; 1001'e çevirmek ayrı, küçük bir iş.)
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

**Karar.** Yeni motor crate'i `gsb-client` (oyun bilmez, politika taşımaz):

- `frame`: `encode`/`encode_into`/`wire_len`; `FrameRx` (iptal-güvenli,
  varsayılan 4 MiB koruma, `with_max`), `FrameTx` (`send` = yaz + flush,
  `send_batch` = tek yazım, `feed`/`flush`, `get_mut` sözleşme dışı
  baytlar için).
- `Conn`: `Stream { rx, tx }` (TCP, TLS, QUIC bi-stream, WebSocket —
  aynı kareler) ya da `Udp(UdpClient)`; `send`, `send_batch`, sınırlı
  `recv(window)` → `Recv::{Frame, Closed, Quiet}`, `into_split` (okuyucu
  ve yazıcı iki görevde — çoğullama yok). Açıcılar: `connect::{tcp,
  tcp_stream, udp, ws}`, `ws::{handshake, handshake_with_max}`,
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

## 6. Taşıma soyutlaması (TCP + rUDP)

```rust
trait Transport: Send + 'static {
    fn bind(self: Arc<Self>, addr) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>>;
}
trait Listener: Send + Sync + 'static {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>>;
    fn local_addr(&self) -> Option<SocketAddr> { None }
    fn close(&self) {}   // §9: rUDP demux'ı sonlandırır (TCP: no-op)
}
struct Endpoint { /* pump görevlerini başlatan tek FnOnce; mailbox'ları taşır */ }
```

`Endpoint`, bağlantı için pump görevlerini başlatır ve kaynaklarını
(socket yarısı, datagram soketi, …) tamamen kendi içinde sahiplenir.
Aktör katmanı pump'ları handle'ler dışında hiç bilmez.

**TCP** (`gsb_net::tcp`): klasik yol — bir socket, bir reader pump
(idle deadline'lı, §3), bir writer pump.

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
tahmin edilebilir bir değer asla kullanılmaz). v1'de kriptografik katman
(HMAC/imza) hâlâ yok — §10 satırı geçerli; ama key artık tahmin
edilemez, dolayısıyla sahte-proof koruması key'in *gibi görünen*
(tahmin edilemez) olmasına dayanır. Çift yönlü mesajlar
aynı boyutta → amplifikasyon oranı ≤ 1; sahte proof, key bilmeden
üretilemez. Kabul (5 B) yalnız doğrulanan proof'a gider: oran 5/18,
sahte proof'a hiçbir şey.

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
  da proof) her `HANDSHAKE_RTO`'da (= taşımanın tek RTO'su, 50 ms)
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
derinliğinde yanlıştır; B4 bir verim ayarı olarak açık kalır, bu
düzeltme olarak değil. (2) *Sunucu tarafında el sıkışma hızlandırma*
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

NAT yeniden bağlanması yeni 4-tuple = yeni el sıkışma =
yeni `ConnectionId` (eski oturum, boşta kalana kadar idle sweep'e
kadar yaşar — sınır: `idle_timeout`).

**Datagram çerçevesi:** `[u8 kind]` — `0` RAW `[u16 op][payload]`
(oyun bandı: kayıp toleranslı, sırasız — snapshot'lar ve MOVE_TO);
`1` REL `[u32 seq][u16 op][payload]` (kontrol bandı: cumulative ACK +
RTO yeniden gönderim, **sıralı teslim** — AUTH/JOIN/LEAVE/HEARTBEAT);
`2` ACK `[u32 next_expected]`; `3` HELLO `[u64 nonce][u64 cookie]`;
`4` FRAG `[u16 mesaj id][u8 index][u8 count][parça]` (yalnız sunucu →
istemci, bütçeyi aşan oyun bandı karesi — aşağıda "MTU").
Band ayrımı: `op 1..=64` (11 hariç) = kontrol (güvenilir), `op ≥ 1000`
= oyun (kayıp toleranslı). Yeniden gönderim: RTO 50 ms; tek
frame asla bırakılmaz, 5 sn ACK ilerlemesi olmazsa BANT ölü ilan edilir
ve oturum biter (`udp/mod.rs` "The REL liveness bound"; eski "250 ms'de
vazgeç" kuralı kaldırıldı), out-of-order penceresi 16; çift frame ACK'lenir ama
yeniden iletmez. ACK'ler demux tarafından oturumun writer'ına **out
kanalı üzerinden** (UDP_ACK frame olarak) verilir — komut kanalı yok,
tek-beklenen-kaynak özdeşliği korunur.

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

**Boşta kapatma (FIN yok):** demux'un `BTreeSet<(Instant, SocketAddr)>`
deadline heap'i (gömlekli geçersiz kılma — girdi yalnız
`son_görülme + idle`'e eşkenken geçerli) + `timeout(min_deadline,
recv_from)`. Sweep, oturuma `ConnIn::ServerClosed` yollar ve oturumu
kaldırır — TCP'nin reader-pump clock'unun (maddeler turu) UDP karşılığı.

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
 bile yalnızca ışık konisi içindekileri gösterme) bu turun kapsamı dışında —
 ROADMAP'e alındı.
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
taşır — stdout parsing'i yok) ve `--orchestrate` (sunucu + P istemci
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
  → RegistryMsg::Shutdown
      → her dispatcher'a RoomOp::Close (yol açma + son leave), sonra senders düşer
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
  → accept loop: JoinHandle.abort()   (belgelenmiş tek sert abort)
  → registry: Shutdown işlenince run() break eder (kendi mailbox klonunu tuttuğu
    için EOF'ı bekleyemezdi — artık beklemez); düşerken Ticker klonunu da
    düşürür — broadcast'i kapatan son halka (§9.1)
  → metrik toplayıcı: ticker'ın broadcast'i kapanınca Closed görür,
    son raporu basıp temiz çıkar (bkz. §12)
```

`Listener::close` kapısı rUDP turunda kullanıldı: `stop()`, registry
Shutdown'ından sonra listener'ı kapatır — TCP'de no-op, rUDP'de demux
görevini sonlandırır (socket klonu + endpoint göndericisi düşer;
writer'lar aktör kaskadıyla çıkar), QUIC'te yeni bağlantıları reddeder
(`set_server_config(None)`) ama canlıları KESMEZ — `Endpoint::close`
akıştaki durdurma bildirimini terk ediyordu (§5.6). Kural: `close` canlı
oturumları kısa kesmez; onlar aktör kaskadıyla, bildirimleriyle biter. Accept loop'un `JoinHandle` ile
abort edilmesi hâlâ v1'in bilinçli kısıtı (demux kapanınca accept de
doğal olarak ölür; kapı, per-listener kibar kapatma için duruyor).

### 9.1 Kapanış kilitlenmesi (S turu, BACKLOG §1 satır 4a)

**Belirti.** U turunda bir MMO ve üç arena-500 loadgen koşusu bitmedi:
`stop()` asılı kaldı. Geçici logla registry'nin bir odanın kontrol
kanalında `cap=0` ile beklediği görüldü. Hata U'dan eski; rUDP turu
yalnızca daha çok eşzamanlı kopuş ürettiği için yüzeye çıkardı.

**`stop()` yolunun tamamı, her `.await` ile** (düzeltme öncesi):

1. `stop()`: `registry.send(Shutdown).await` — registry posta kutusu
   (4096); registry onu tick'ten bağımsız boşaltır.
2. `stop()`: `http.abort()`, `ticker.abort()` — beklemesiz. Ticker
   görevinin göndericisi düşer, ama registry bir `Ticker` klonu tuttuğu
   için broadcast **açık kalır**; odalar artık tick almaz, yani kontrol
   kanallarını bir daha boşaltmaz.
3. `stop()`: her listener `close()`, her accept `abort()` — beklemesiz.
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
kalabilir" sınırlılığı aynen geçerli. İstemci tel baytları değişmedi.
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

## 10. v1 kısıtları

| Kısıt | Neden | Yol |
|---|---|---|
| ~~Yayın = tam snapshot (grup başına)~~ *(kapandı — delta modu: AOI/`spatial` (§8.1 "Delta yayın"), sharded × spatial, takım sisi odası `TeamRoom::with_delta` (T) ve team × sharded `ShardedTeamRoom::with_delta` (W1); açık, PVS ve düz sharded odalar full; zarf ve istemci kuralları `crates/gsb-kit/proto/kit.proto`)* | Full kendi kendine yeter; delta modunda yakınsama keep-alive full'ıyla | değer düzeyinde delta — BACKLOG A22 |
| Keepalive snapshot'ı (varsayılan 1 Hz) | Son paketi kaybeden istemci kalıcı bayat kalmasın | `keepalive_hz` (tick hızını aşamaz: oda kendi tick hızından hızlı keepalive yapamaz; yüksek değer `KeepaliveRate` ile reddedilir); 0 ile kapatılabilir |
| `max_snapshot_bytes` aşımında yalnızca uyarı (grup başına bir kez) + `snap_overflows` sayacı | Payload çekirdekte bölünmez; rUDP'de eşiği aşan kare **taşımada parçalanır** (§6 "MTU"), yani sayaç artık bant genişliği/parçalanma sinyali — kayıp sinyali istemcinin `frag_dropped_incomplete`'i | uyarıya göre grubu böl (AOI) / hızı düşür (§8) |
| Oda hizi global tick hızını tam bölmeli | broadcast ticker + adım atlama (`run_every`) | global hız tek kaynak; dinamik adaptif tick gelecek |
| Accept loop abort | `Listener::close` rUDP turunda eklendi (demux kapatma); accept abort hâlâ kaskadın son halkası | §9 |
| rUDP: **congestion control yok** | UDP'de sunucu pps'sini sınırlandıran şey yalnız oda bütçesi; loopback ölçümünde sorun yok, gerçek ağda retransmission fırtınası riski | token bucket (oturum başına) — ROADMAP P1 |
| rUDP: **şifreleme/imza yok** (HMAC katmanı değil) | v1 kapsamı; ama **cookie key artık tahmin edilemez** — konfigürasyondaki `cookie_key` ya da (varsayılan) OS entropisinden (`getrandom`) 16 bayt, sessiz zayıf geri düşüş yok (entropi yoksa süreç başlatmayı reddeder). Sahte-proof/amplifikasyon koruması key'in gizliliğine değil tahmin edilemezliğine dayanır; ağ şifrelemesi ayrı katman | DTLS ya da uygulama katmanı TLS — ROADMAP P1 |
| rUDP: parçalama **yalnız oyun bandında, yalnız sunucu → istemci**, mesaj başına en çok 16 parça (varsayılan bütçede 23 472 B); aşan kare atılır + sayılır; kontrol bandı parçalanmaz (aşan kontrol karesi oturumu bitirir) | ölçülen en büyük full 10 267 B (arena 1000; W2'de savaş 1000'in keep-alive full'ü ~18,5 KB — CROSS-SHARD §8b.8); yeniden gönderim yok — bant kendini iyileştirir; istemci durumu sabit sınırlı (§6 "MTU", SECURITY §4.1) | daha büyük kareler için grup bölme (AOI) — §8 |
| rUDP: SO_RCVBUF ayarı yok | tokio 1.53.1 `UdpSocket`'inde buffer boyutu setter'ı yok (raw fd gerekir) | tokio setter'ı geldiğinde / raw fd wrapper |
| rUDP: NAT yeniden bağlanması = yeni el sıkışma + yeni `ConnectionId`; eski oturum idle sweep'e kadar yaşar (≤ `idle_timeout`) | stateless cookie, 4-tuple anahtarlı oturum | istemci tarafı reconnect + sunucu tarafı kimlik eşleme (auth katmanı) |
| Oda kapasitesi **vardır**: `max_players` (vars. `Some(10_000)` = ölçülen duvar) + sunucu geneli `max_connections` (vars. `Some(100_000)`) | koruma katmanı (bu tur); semantiği: nazik reddi — oda dolu `ERROR 8` (bağlantı yaşar), cap `ERROR 9` + kapatma; çünkü sınır, ölçülen sayılara dayandı (C1 duvarı 9–10k), tahmine değil | sınırsız oda gerekirse `None` (0 = sınırsız) |
| join/leave tick sınırında işlenir (≤ 1 tick gecikme) | CONTROL fazı determinizmi (bilinen tick'te spawn/leave) | v1'de kabul edilen özellik; gerekirse tick-içi hızlı yol |
| Girdi kaybı **yalnızca göndericinin kendi kanalında** ve **atfeli**: connection actor `try_send` Full'u kendi metrik örneğinde sayar (`actions_dropped`, `actions_dropped_top`); odaya çeken READ fazı sınırlı çekmedir — bağlantı başına tick bütçesi 16 + oda çekme bütçesi 65536, oda çektiği aksiyonu asla atmaz | flooding bir bağlantı başkasının aksiyonunu evicted edemez (eski merged-list en eskiyi atıyordu); hasar saldırgana sınırlı | sürekli (sn başına) rate-limit (tur başına bütçe zaten sınırlayıcıdır) |
| Tek process | v1 kapsamı | ~~§8.4~~ §8 "sonraki adımlar" madde 4; süreç içi bölme §8.2; süreçler/makineler arası: DISTRIBUTED (`ShardLink` tasarımı) |
| Oturum zaman aşımı **reader pump'ta** (read deadline), registry'de değil | çünkü saati tutan yer, stream'i bekleyen yeridir — registry'ye son-görülme damgası ikinci bir beklenen kaynak/timer çıkarırdı (§3); 30 sn varsayılan, 0 = kapalı | oyun seviyesi oturum politikası (reconnect'de yeniden auth vb.) registry katmanı |
| `sint32` (tam sayı) koordinat, `f32` simülasyon | Demo sadeliği | float veya mm cinsinden int (sabit nokta) |
| ~~Güvenlik yüzeyi minimal: AUTH no-op~~, sn-başına **geçerli girdi** hacim sınırı yok (cap'ler var: bağlantı cap + oda cap + tur-başına girdi bütçesi) *(AUTH kısmı kapandı — ticket kancası `TicketAuth` (`gsb-core/src/auth.rs`; yapılandırılmazsa `Auth.name` olduğu gibi kabul: yalnız geliştirme yolu, SECURITY §4b), AUTH deneme sınırı + pre-auth kare bütçesi + HEARTBEAT kısması + unauthed cap (SECURITY §3–§4), TLS (SECURITY §2))* | saniyede kaç aksiyonun meşru olduğu oynanış parametresi — kullanıcı kararı | ~~`Authenticator` trait'i +~~ rate-limit — BACKLOG E1 → D1 |

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
                    ticker kapalıysa → son rapor + temiz çıkış
                                                            │
                                     ┌──────────────────────┴──────────────────┐
                                     ▼                                         ▼
                          MetricSink::Log                            MetricSink::Channel
                          (tracing info: gsb-metric scope=.. k=v)    (→ uygulama/
                          RUST_LOG=info ile görünür; yoksa sessiz)   yük üreticisi/test)
```

**Neden bounded + `try_send`:** room actor'ünün tick gövdesine **hiç await
eklenmez** (spec'in sert şartı; tek await hâlâ `tick_rx.recv()`'tir).
`UnboundedSender::send` senkrondur ama sınırsız bellek tutar (toplayıcı
sarkarsa sızma); `bounded send` ise `Future` → tick içinde bekler. Çözüm
`bounded(4096)` + **senkron `try_send`** (Future değil): kanal doluyken
örnek atılır ve üreticinin `metrics_dropped` sayacı artar — kümülatif
sayaçlar için zararsız (bkz. ROADMAP "metrik düzeltme + AOI turu").
Örnekler sabit boyutlu ve oda başına adımda en fazla bir tane olduğundan
4096 derinlik geniş bir marj bırakır; en kötü hâl örnek kaybıdır, tick
durdurulamaz.

**Ne ölçülür (neden):**

| Kapsam | Metrik | Sorulan soru |
|---|---|---|
| oda | `steps`, `hz` (Δadım/örnek-aralığı), `late_*` (tick gecikmesi), `step_*` + `step_hist` (adım süresi dağılımı: tick bütçesinin **oranları**, log-2 merdiven 1/128×…32×; `(1,1)` kenarı = bütçe = aşım sınırı) | konfigure hıza ulaşılıyor mu? adım bütçesinin (33 ms @30 Hz) neresindeyiz? bütçe aşılıyor mu? |
| oda | `lagged_events/ticks` | broadcast tamponu aşıldı mı (oda tick kaçırıyor mu)? |
| oda | `dropped`, `keepalive_resends` | fan-out backpressure'ı (yavaş istemci) var mı? |
| oda | `snapshots`, `snap_bytes_s`, `snap_bytes_max`, `shipped_bytes`/`shipped_s`, `shipped_frames`, `private_frames` | yayın yükü: kaç snapshot, kaç bayt, tepe paket boyutu (MTU/hazırlık sinyali), kaç KARE ve bunların kaçı özel (datagram taşıması bayt kadar PAKET ile de sınırlı; `shipped_bytes/shipped_frames` = ortalama kare boyu, `shipped_frames − private_frames` = fan-out'un yayın yarısı) |
| oda | `groups`, `members`, `max_group`, `joins`, `leaves` | oda doluluğu ve churn |
| registry | `rooms`, `conns`, `opens`, `closes`, `joins`, `leaves` | bağlantı/oda sayısı ve akışı (100k hedefinin sayacı) |
| conn | `bytes_in/out`, `frames_in/out` (delta), `actions_dropped` (net toplam, kümülatif)
| istemci başına bant; net toplam = room fan-out (baskın) + kontrol |
| conn | `actions_dropped_top` (raporda: en çok düşürmüş 5 bağlantı, `c{n}:sayı`)
| düşen girdi **kime ait** (flooding atfesi — koruma katmanı; §4) |
| net | `server_closes` — sebep başına kümülatif (`ServerClose`: `idle_timeout`, `write_stall`, `rel_dead`, `violation_budget`, `preauth_budget`, `stream_rejected`, `conn_cap`, `unauth_cap`, `superseded`, `room_gone`, `outbound_dead`); Prometheus'ta TEK aile `gsb_net_server_closes_total{reason=…}` | sunucu hangi oturumları KENDİ kararıyla, neden bitirdi? İstemci-tarafı son ve shutdown sayılmaz (SECURITY §3.6). Tıkanmış soket ERROR taşıyamadığından istemci sayaçları bunu göremez — `errors=0` bir yük ölçümünde dökülen yarım istemciyi gizleyebiliyordu |

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
| SUM | `lagged_*`, `dropped`, `keepalive_resends`, `snapshots`, `snap_overflows`, `snap_records`, `shipped_bytes`, `shipped_frames`, `private_frames`, `joins`, `leaves`, `resumes`, `resume_rejected_stale`, `detach_expired_*`, `detach_forced`, `effects_*` ve `migrations_*` aileleri (küçük paket), `team_*` ailesi (W2), `requests_*` ailesinin tamamı, `metrics_dropped` | Ayrık iş üzerindeki kümülatif sayaçlar. (Bir göç kaynağında `migrations_out`, hedefinde `migrations_in` olarak bir kez sayılır: katlanmış ikili eşit çıkmalı, birbirine eklenmez.) |
| SUM | `dropped_s`, `snap_bytes_s`, `shipped_s` | Shard başına hesaplanmış bir ORAN ortalanamaz: sayaçlar aynı duvar saati üzerinde ayrıktır, odanın oranı toplamlarıdır (ortalamak 4 shard'lık odada kaybın dörtte birini raporlardı). |
| SUM | `groups`, `members`, `detached`, `pending_requests` | Gauge, ama **bölünmüş** gauge — shard'lar odanın bağlantılarını, gruplarını, park edilmiş oturumlarını ve uçuştaki isteklerini PAYLAŞTIRIR, yani odanın değeri toplamdır. Karşı örnek `max_group`/`snap_bytes_max`: bunlar bir popülasyon değil, popülasyon ÜZERİNDE bir uçtur. |

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
olanı alır (kapanış sonrası son rapor yalnız ekleyebilir). Kabul
edilen bedel: son örnek de `try_send`'dir, kanal kapanış anında DOLUysa
hüküm düşer ve düşüş hiçbir yerde sayılmaz (aktör gitmiştir) — 4096
derinlik ve tick başına boşaltmayla bir tick içinde binlerce kapanış
gerekir. Log satırı: `server_closes=<toplam>` + `server_close_<reason>=N`;
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

**Kullanım:** `gsb-server` çalışırken `RUST_LOG=info` → metrik satırları
logda; `gsb_server::start_server_metrics(cfg, tx)` → raporlar kanaldan
programatik (yük üreticisi ve testler bu yoldan kullanır). Yük testi
sayıları ve ilk doyma analizi: CHANGELOG "Kapatılanlar (metrik + yük turu)".

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
