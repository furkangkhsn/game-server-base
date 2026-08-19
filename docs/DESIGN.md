# gsb Mimari Tasarım Dokümanı

Bu doküman, gsb'nin tasarım kararlarını, ölçekleme hedeflerini, bilinçli
kısıtlarını ve yol haritasını içerir. Kod İngilizce yorumlanır; bu doküman
ve sohbet Türkçe'dir.

## 1. Amaç ve kapsam

**Amaç:** MOBA/MMORPG sınıfı, çok oyunculu, gerçek zamanlı oyunlar için
100k+ eşzamanlı bağlantıyı hedefleyen bir sunucu temeli.

**Kapsam dışı (v1):** oyun mantığı (demo hariç), delta/AOI tabanlı yayın,
kompresyon, kalıcılık, cross-server (cluster), yük dengeleyici.

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
- **Girdi izolasyonu:** her bağlantının kendi `Action` kanalı var; READ
  fazı `try_recv` ile bloksuz çeker. `max_pending_actions` aşıldığında
  **en eski** aksiyonlar atılır (oda, gerçek zamanın gerisinde kalmışsa bile
  sınırlı kalır).
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
  - Snapshot payload'u `RoomConfig::max_snapshot_bytes`'i aşarsa uyarı
    loglanır (rUDP'de MTU hazırlığı: aşırı snapshot datagram'a sığmaz;
    sürekli uyarı = grubu bölme/AOI zamanı, §8).
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
  HEARTBEAT, HEARTBEAT_ACK, ERROR); `1000+` oyun bandı
  (MOVE_TO=1000, WORLD_SNAPSHOT=1003; PRIVATE=1004 `RoomLogic::private`
  için ayrılmış, demo kullanmaz; 1001/1002 boş — eski ENTITY_SPAWNED /
  ENTITY_REMOVED kaldırıldı, üyelik snapshot'ta var olmaya indirgendi).
- **Seriştirme:** protobuf. Rust tarafında `prost`, Unity tarafında
  `Google.Protobuf` — aynı `.proto` dosyaları her iki tarafta kullanılır.
  Mesajlar `MessageTable`'da opcode→(de)koducu olarak kayıt edilir; tablo
  başlangıçta bir kez kurulur ve `Arc` ile salt okunur paylaşılır (hiçbir
  yazma, hiçbir kilit).
- `FrameBody { op, payload: Bytes }` — `Bytes` sayesinde payload kopyasız
  akar (socket → actor → oda → yayın, tek kopya).

## 6. Taşıma soyutlaması (rUDP yolu)

```rust
trait Transport: Send + 'static {
    fn bind(self: Arc<Self>, addr) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>>;
}
trait Listener: Send + Sync + 'static {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>>;
    fn local_addr(&self) -> Option<SocketAddr> { None }
}
struct Endpoint { /* pump görevlerini başlatan tek FnOnce */ }
```

`Endpoint`, bağlantı için reader/writer pump görevlerini başlatır ve
kaynaklarını (socket yarısı, datagram soketi, …) tamamen kendi içinde
sahiplenir. Aktör katmanı pump'ları iki `JoinHandle` dışında hiç bilmez.
rUDP eklendiğinde: `UdpTransport::bind` bir `UdpListener` döndürür;
`UdpListener::accept` bir datagram başına "endpoint" (aslında bir mantıksal
oturum) üretir; pump, `ConnIn::Frame`'i aynı mailbox'a yollar. **Aktör
katmanında değişiklik sıfırdır.**

## 7. ECS katmanı

- `bevy_ecs` **standalone** olarak kullanılır (Bevy engine'ı değil). Oda
  actor'ü `World`'ün **tek** borçlusu olduğundan ECS'nin çok-ithal
  (multi-borrow) makinesi zaten güvenli çalışır; bizim ek kısıtımız — tek
  thread, sıralı sistemler — bunu daha da basitleştirir.
- `gsb-ecs::System` trait'i el yapımıdır (`fn run(&mut self, &mut World,
  &SystemCtx)`): bevy'nin `SystemParam`/scheduler mekanizması hot path'te
  gereksiz kit olarak dururdu. `SystemRunner` insertion-order çalıştırır.
- **Değişim algılama (dirty tracking):** bevy 0.19'da event/observer
  API'si yeniden tasarlandığı için hot path'te bilinçli olarak bevy
  *change-detection*'ı kullanılmıyor; ayrıca ayrı bir versiyon bileşeni
  de yok — eski `EntityVersion` + `bump()` mekanizması denetim turunda
  kaldırıldı (F1'den beri okuyucusuz kalmıştı: karar wire içeriğine
  taşınınca versiyonun tek okuyucusu giderilmiş, ama yazma tarafı ve
  bu maddenin de içinde olduğu 4 doküman onu hâlâ *kullanımda* olan
  mekanizma gibi anlatıyordu). Değişim sinyali artık **wire içeriğinin
  kendisi**: oyun mantığı, grup snapshot'ını yeniden üretip
  üretmeyeceğini **kendisi** karar verir ve bu kararın defteri
  **grup başına** tutulmalıdır (§4): demo'da son yayınlanan
  snapshot'ın **wire içeriği** (`entity → (x, y)`, wire'ın tam sayı
  konumlarına kesilmiş) tutulur; içerik değiştiyse (konum **veya**
  üyelik) snapshot yeniden kodlanır — içeriği değiştirmeyen hiçbir
  yazım yayınlatmaz (bant israfı yok). Gerekirse (örn. delta yayını,
  P2) bir versiyon mekanizması o özellikte yeniden getirilebilir. Eski
  bağlantı başına `last_sent` haritası ve spawn/remove olayları
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
- **Delta yok, geçmiş yok:** snapshot'lar kendi kendine yeter; paket kaybı
  bir sonraki snapshot'la kendiliğinden telafi edilir. Sıra/güvence ihtiyacı
  yok — istemci, `sequence`'i (global tick indeksi) ≤ son kabul edilen olan
  snapshot'ı atar (sıralama + tekrar güvenli).
- **Değişiklik yoksa yayın durur** (oyun mantığı `snapshot` → `false`);
  keepalive (varsayılan 1 Hz, `RoomConfig::keepalive_hz`) değişmeyen
  grupların son önbellekli snapshot'ını yeniden gönderir — son paketini
  kaybeden istemci kalıcı bayat kalamaz.
- Bağlantı başına tek batch + `try_send`: yavaş istemci sunucuyu
  yavaşlatmaz; atılan batch'in maliyeti 1 snapshot bayatlık.
- `max_snapshot_bytes` aşımı uyarı loglanır (rUDP MTU hazırlığı).
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
  istemcinin dünya görüşü son kabul ettiği snapshot'tır; delta, geçmiş ve
  out-of-band "kimlik yeniden eşleme" mesajı yoktur. Değişmezin
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
2. **AOI + oda içi Visibility trait'i** — MMORPG'ler için büyük oda
   segmentasyonu; fan-out'u O(görünürlük kümesi) yapar.
3. **Kompresyon (zstd)** — frame batch'leri üzerine ek bir transport
   seçeneği (uzunluk öneki zaten transport'un malı).
4. **Oda bölme/birleştirme (sharding)** ve cross-region.

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
  `gsb_game::aoi::tests` + `tests/aoi.rs`).
- **Ölçülen ticaret:** AOI ~9× kodlama maliyeti taşır (her kayıt 9 komşu
  bloğa girer) ama bant genişliğini O(entity) → O(görünürlük) yapar; hücre
  boyutu küçükçe bant kazancı %80+ (1000/2000). Break-even + yeni darboğaz
  (adım süresi/CPU) ROADMAP "Kapatılanlar (metrik düzeltme + AOI turu)"
  bölümünde ölçülmüş. 10k+ ölçekli oda segmentasyonu + `Visibility` trait'i
  hâlâ P2 (adım 2'nin kalanı).

## 9. Kapanma (shutdown) kaskadı

Abort'siz, kanal kapanmalarına dayalı:

```text
ServerHandle::stop
  → RegistryMsg::Shutdown
      → her dispatcher'a RoomOp::Close (yol açma + son leave), sonra senders düşer
      → her bağlantının inbox'ına ConnIn::Shutdown  (spawn'lu gönderim)
      → her odaya kontrol kanalından RoomControl::Shutdown (bir sonraki tick'te işlenir)
  → ticker.abort()   (broadcast kapanır = geri sigorta: kontrol Shutdown'ını
                     görememiş her oda, recv'de Closed görüp temiz çıkar)
  → connection actor'ler çıkar → in_tx/out_tx düşer
      → reader pump: send hatası → çıkar
      → writer pump: kanal kapanır → çıkar + socket close
  → accept loop: JoinHandle.abort()   (belgelenmiş tek sert abort)
  → registry: Shutdown işlenince run() break eder (kendi mailbox klonunu tuttuğu
    için EOF'ı bekleyemezdi — artık beklemez)
  → metrik toplayıcı: ticker'ın broadcast'i kapanınca Closed görür,
    son raporu basıp temiz çıkar (bkz. §12)
```

Accept loop'un `JoinHandle` ile abort edilmesi v1'in bilinçli bir kısıtıdır:
listener'ı "kibarca kapatmak" için transport trait'ine kapatma yöntemi
eklemek gerekir; bu, rUDP'ye kadar ertelendi (yeni bir transport eklerken
birlikte ele alınacak).

## 10. v1 kısıtları

| Kısıt | Neden | Yol |
|---|---|---|
| Yayın = tam snapshot (grup başına) | Basitlik + düşmeye tolerans | delta → AOI (§8) |
| Keepalive snapshot'ı (varsayılan 1 Hz) | Son paketi kaybeden istemci kalıcı bayat kalmasın | `keepalive_hz` (tick hızını aşamaz: oda kendi tick hızından hızlı keepalive yapamaz; yüksek değer `KeepaliveRate` ile reddedilir); 0 ile kapatılabilir |
| `max_snapshot_bytes` aşımında yalnızca uyarı (grup başına bir kez) | rUDP MTU hazırlığı; snapshot'lar bölünmüyor | uyarıya göre grubu böl (AOI) / hızı düşür (§8) |
| Oda hizi global tick hızını tam bölmeli | broadcast ticker + adım atlama (`run_every`) | global hız tek kaynak; dinamik adaptif tick gelecek |
| Accept loop abort | Trait'e close eklemek rUDP ile birlikte | §9 |
| Oda kapasitesi yok (sonsuza kadar oyuncu) | Demo oda | `RoomConfig.max_players` + doluluk yanıtı |
| join/leave tick sınırında işlenir (≤ 1 tick gecikme) | CONTROL fazı determinizmi (bilinen tick'te spawn/leave) | v1'de kabul edilen özellik; gerekirse tick-içi hızlı yol |
| Girdi `try_send` (kanal doluyken atılır) | oyuncu bazlı izolasyon, oda bloke olmaz | bağlantı başına girdi hız sınırı (rate-limit) |
| Tek process | v1 kapsamı | §8.4 |
| Heartbeat → yalnızca ack (oturum zaman aşımı yok) | v1 kapsamı | registry'de son-görülme zaman damgası |
| `sint32` (tam sayı) koordinat, `f32` simülasyon | Demo sadeliği | float veya mm cinsinden int (sabit nokta) |
| Güvenlik yüzeyi minimal: AUTH no-op, rate-limit yok, bağlantı limiti yok | v1 kapsamı | `Authenticator` trait'i + rate-limit + cap |

> Not: Önceki sürümlerdeki iki kritik hata — sonradan giren oyuncunun
> dünyayı görmemesi ve registry'nin oda cevabını beklerken tüm sunucuyu
> bloke etmesi — kapatıldı (§3, §7). `RoomId(0)` sentinel'ı kaldırıldı,
> `ConnInfo.room` artık `Option<RoomId>`.

## 11. Test stratejisi

- **gsb-protocol:** frame encode/decode, bozuk çerçeve, tablo round-trip,
  bilinmeyen opcode.
- **gsb-lint:** yorum soyma (satır/blok/iç içe), satır numarası korunumu.
- **gsb-net:** gerçek loopback TCP üzerinde framing round-trip, çoklu
  frame yeniden derleme + EOF, aşırı boyutlu length-prefix reddi.
  (Testler echo-peer kullanır; pasif peer'da TCP yarı kapanışı davranış
  farkı yaratır.)
- **gsb-core:** global ticker + oda actor — tick fazları, `dt` üst sınırı
  (catch-up), `run_every` ile yavaş oda atlama, `Lagged` sonrası catch-up +
  ticker kapanışında temiz çıkış (sentez zaman damgalarıyla manuel
  broadcast besleme, kilit yok). Registry: join→leave→rejoin dizisi
  (gözlemci bağlantı üzerinden oyuncu sayısı doğrulanır — stale leave
  sayacı geri düşürmemeli), oda imhası bildirimi + imha sonrası join
  reddi, bölünmeyen oda hızı reddi (`TickRate`), temiz shutdown
  (registry handle'ı çözülür) — **gerçek 60 Hz ticker** ile.
  Grup mekanizması: `GroupKey = ConnectionId` mantıkla gruplar birbirinden
  yalıtılır (bir grubun snapshot'ı başka bağlantıya asla sızmaz), private
  frame yalnızca hedef bağlantıya gider; değişmeyen grup sessiz kalır,
  keepalive kadansında önbellekli snapshot yeniden gönderilir; **her tick
  değişen dünyada aynı tick'te değişen her grup yayınlanır** (grup başına
  defterle — oda tarafının grup-başına davranışına regresyon; mantık
  tarafındaki paylaşımlı defter yanlış kullanımı bu testle yakalanamaz:
  oda, meşru sessizlik ile ihlali ayırt edemez, o kullanım sözleşme
  metniyle korunur — §4 Tanı maddesi).
- **gsb-game:** gecikmeli giriş — hareketsiz A'nın olduğu odaya B girerse B,
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
- **gsb-server (e2e):** process-içi sunucu (ephemeral port) + gerçek TCP
  istemci: AUTH → JOIN → MOVE_TO → `WORLD_SNAPSHOT` akışı: önce kendi
  entity'sini görür, hareketten sonra snapshot'ta konumunu **değişmiş**
  görür. Tüm yol tek test: pump → bağlantı actor → registry → dispatcher →
  oda → bevy world → hareket sistemi → grup snapshot'ı → writer pump.
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
| oda | `snapshots`, `snap_bytes_s`, `snap_bytes_max`, `shipped_*` | yayın yükü: kaç snapshot, kaç bayt, tepe paket boyutu (MTU/hazırlık sinyali) |
| oda | `groups`, `members`, `max_group`, `joins`, `leaves` | oda doluluğu ve churn |
| registry | `rooms`, `conns`, `opens`, `closes`, `joins`, `leaves` | bağlantı/oda sayısı ve akışı (100k hedefinin sayacı) |
| conn | `bytes_in/out`, `frames_in/out` (delta) | istemci başına bant; net toplam = room fan-out (baskın) + kontrol |

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

**Kullanım:** `gsb-server` çalışırken `RUST_LOG=info` → metrik satırları
logda; `gsb_server::start_server_metrics(cfg, tx)` → raporlar kanaldan
programatik (yük üreticisi ve testler bu yoldan kullanır). Yük testi
sayıları ve ilk doyma analizi: ROADMAP "Kapatılanlar (metrik + yük turu)".

## 13. Derleme zamanı korumaları

- `unsafe_code = "forbid"` — her crate'te.
- `gsb-lint` — 6 crate'in `build.rs`'inde (lint crate'i hariç) select/kilit
  desenleri build hatası; kapsam `src/` + `tests/` + `examples/`.
- `cargo clippy --workspace --all-targets` temiz.
- `edition = "2024"` (Rust 1.95).
