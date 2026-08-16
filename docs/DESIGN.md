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
  (`GroupKey`: `Eq + Hash + Clone`; demo'da `()` = oda başına tek grup,
  `ConnectionId` = bağlantı başına grup — arayüz ikisini de taşır). Her tick
  oda her grup için snapshot'ı **bir kez** `RoomLogic::snapshot()` ile
  kodlar, `freeze()`'ler ve üyeleriyle `Bytes` (Arc refcount) klonu olarak
  paylaşır — payload **asla** bağlantı başına kopyalanmaz, bağlantı başına
  kodlama yoktur (eski `OutSink` + `last_sent` yapısı kaldırıldı). Oda
  grup başına durumu (üyeler + son gönderilen snapshot) bir `HashMap`'de
  tutar; bu yüzden oda actor'ü `RoomActor<W, G>`'dir ve registry/factory
  `G` üzerinden geniktir.
  - **"Değişiklik yok"** kararını oyun mantığı verir (`snapshot` →
    `false`); tanım **üyelik değişimini (join/leave) da** kapsar. Hiçbir
    grup değişmediyse oda o tick'te **hiçbir şey göndermez** — tek istisna
    **keepalive**: her `tick_hz / keepalive_hz` (varsayılan 1 Hz) odasal
    adımda değişmeyen her grup, son önbellekli snapshot'ını yeniden
    gönderir (yeniden kodlama yok, önbellek klonu). Aksi hâlde son
    paketini kaybeden istemci kalıcı olarak bayat kalırdı.
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
- **Dirty tracking:** bevy 0.19'da event/observer API'si yeniden
  tasarlandığı için hot path'te bilinçli olarak *değişim algılama*
  kullanılmıyor. Bunun yerine açık, deterministik `EntityVersion`
  component'i: her anlamlı mutasyonda `bump()`. Oyun mantığı, grup
  snapshot'ını yeniden üretip üretmeyeceğini bu versiyonlarla (ve üyelik
  kümesiyle) kendisi karar verir: demo'da son yayınlanan snapshot'ın
  `(entity → versiyon)` kümesi tutulur; küme değiştiyse (bump **veya**
  join/leave) snapshot yeniden kodlanır. Eski bağlantı başına `last_sent`
  haritası ve spawn/remove olayları kaldırıldı: üyelik, snapshot'ta var
  olmaya indirgendi (§4/§8).
- `EntityId = u64` core'da ECS'sizdir; oyun crate'i `Entity::to_bits()` /
  `from_bits()` ile çevirir.

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
```

Accept loop'un `JoinHandle` ile abort edilmesi v1'in bilinçli bir kısıtıdır:
listener'ı "kibarca kapatmak" için transport trait'ine kapatma yöntemi
eklemek gerekir; bu, rUDP'ye kadar ertelendi (yeni bir transport eklerken
birlikte ele alınacak).

## 10. v1 kısıtları

| Kısıt | Neden | Yol |
|---|---|---|
| Yayın = tam snapshot (grup başına) | Basitlik + düşmeye tolerans | delta → AOI (§8) |
| Keepalive snapshot'ı (varsayılan 1 Hz) | Son paketi kaybeden istemci kalıcı bayat kalmasın | `keepalive_hz`; 0 ile kapatılabilir |
| `max_snapshot_bytes` aşımında yalnızca uyarı | rUDP MTU hazırlığı; snapshot'lar bölünmüyor | uyarıya göre grubu böl (AOI) / hızı düşür (§8) |
| Oda hizi global tick hızını tam bölmeli | broadcast ticker + adım atlama (`run_every`) | global hız tek kaynak; dinamik adaptif tick gelecek |
| Accept loop abort | Trait'e close eklemek rUDP ile birlikte | §9 |
| Oda kapasitesi yok (sonsuza kadar oyuncu) | Demo oda | `RoomConfig.max_players` + doluluk yanıtı |
| join/leave tick sınırında işlenir (≤ 1 tick gecikme) | CONTROL fazı determinizmi (bilinen tick'te spawn/leave) | v1'de kabul edilen özellik; gerekirse tick-içi hızlı yol |
| Girdi `try_send` (kanal doluyken atılır) | oyuncu bazlı izolasyon, oda bloke olmaz | bağlantı başına girdi hız sınırı (rate-limit) |
| Tek process | v1 kapsamı | §8.4 |
| Heartbeat → yalnızca ack (oturum zaman aşımı yok) | v1 kapsamı | registry'de son-görülme zaman damgası |
| `sfixed32` (tam sayı) koordinat, `f32` simülasyon | Demo sadeliği | float veya mm cinsinden int (sabit nokta) |
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
  keepalive kadansında önbellekli snapshot yeniden gönderilir.
- **gsb-game:** gecikmeli giriş — hareketsiz A'nın olduğu odaya B girerse B,
  aynı tick'in `WORLD_SNAPSHOT`'ında **A dahil tüm dünyayı** görür; A
  hareket edince B, sonraki snapshot'larda yeni konumu görür (üyelik ve
  durum snapshot'ta varlıkla/val ile ifade edilir). Stale leave —
  rejoin'dan gecikmeyle gelen eski leave, yeni entity'yi öldüremez
  (hedefli MOVE_TO + snapshot ile doğrulanır; snapshot'ta yeni entity var,
  eski entity yok). Kare hızından bağımsızlık — tek 60 Hz saat altında iki oda
  (60 Hz `run_every=1` ve 15 Hz `run_every=4`), gerçek hareket sistemi:
  5.0 s simülasyon süresi her iki odada aynı mesafe (f64 gözlem kanalı —
  i32 wire, karşılaştırmayı kuantum gürültüsü altında boğardı).
- **gsb-server (e2e):** process-içi sunucu (ephemeral port) + gerçek TCP
  istemci: AUTH → JOIN → MOVE_TO → `WORLD_SNAPSHOT` akışı: önce kendi
  entity'sini görür, hareketten sonra snapshot'ta konumunu **değişmiş**
  görür. Tüm yol tek test: pump → bağlantı actor → registry → dispatcher →
  oda → bevy world → hareket sistemi → grup snapshot'ı → writer pump.

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

## 12. Derleme zamanı korumaları

- `unsafe_code = "forbid"` — her crate'te.
- `gsb-lint` — 6 crate'in `build.rs`'inde (lint crate'i hariç) select/kilit
  desenleri build hatası; kapsam `src/` + `tests/` + `examples/`.
- `cargo clippy --workspace --all-targets` temiz.
- `edition = "2024"` (Rust 1.95).
