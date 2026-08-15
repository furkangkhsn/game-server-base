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

Tek kural: **her aktörün tek `await`'i mailbox'ına gelen mesajı okumaktır.**

Bu kural üç somut biçimde uygulanır:

1. **Hiçbir `tokio::select!` yok.** Çoklu bekleme ihtiyacı, beklenecek her
   kaynak için **ayrı bir görev** açılarak çözülür (pump görevleri, pacer
   görevi). "Birden fazla kaynağı tek görevde multiplex etme" deseni bu
   mimaride var olmaktan çıkar; bu, büyük sistemlerde bug'ların ana kaynağıdır.
2. **Hiçbir kilit yok.** Durum, ait olduğu aktörün (veya görevin) yerel
   değişkenlerinde yaşar. Aktörler arası her değer (mailbox, oneshot yanıt,
   frame batch) kanallarla *taşıma* (move) edilir; paylaşım yoktur.
3. **Kural derleme zamanında denetlenir.** `gsb-lint` her gsb crate'inin
   `build.rs`'inde `src/**/*.rs`'i tarar; `tokio::select`, `futures::select`,
   `std::sync::Mutex`, `std::sync::RwLock`, `parking_lot` kalıplarıyla
   karşılaşırsa derleme **hata** ile biter. Yorum/doküman metni sayılmaz
   (önce yorumlar soyulur, satır numaraları korunur).

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
```

- **Bağlantı başına 3 görev:** reader pump (socket → `ConnIn::Frame`),
  connection actor (durum makinesi: auth → join → forward), writer pump
  (`FrameBatch` → socket). Pump görevlerinin her biri de tek kaynaktan
  bekler: stream'in bir öğesi ya da kanal mesajı.
- **Oda başına 2 görev:** pacer (sabit tempoda `RoomMsg::Tick`; sapma
  düzeltmeli — saati yakalayamazsa tick atlar) + room actor.
- **Registry:** sunucunun kontrol düzlemi. Oda tablosu
  (`RoomId → mailbox + pacer`), bağlantı tablosu
  (`ConnectionId → {room, inbox}`) ve oyun mantığını core'e sokan
  `RoomFactory<W>` kapağı.
- **Accept loop:** `ConnectionId` üretir, pump görevlerini başlatır,
  `ConnOpened`'ı **actor'ü başlatmadan önce** registry'e gönderir (ilk
  istemci frame'ine karşı sıralama garantisi).

## 4. Oda tick'i: 4 faz

Room actor'ün tick gövdesi **senkron**'dur (tek `await`'i mailbox recv'i):

```text
Pacer ──Tick──▶ 1. READ:     pending aksiyonları boşa dök (mem::take)
                 2. CONVERT:  aksiyon → component yazıları  (RoomLogic::ingest)
                 3. SYSTEMS:  sıralı oyun sistemleri        (RoomLogic::update)
                 4. BROADCAST: dirty entity → frame → bağlantı kanalları
                                  (RoomLogic::broadcast + OutSink::flush)
```

- `max_pending_actions` aşıldığında **en eski** aksiyonlar atılır (oda,
  gerçek zamanın gerisinde kalmışsa bile sınırlı kalır).
- `OutSink` yayın fazında bağlantı başına bir `Vec<FrameBody>` tamponlar ve
  tick sonunda **bağlantı başına tek `try_send`** yapar. Fan-out maliyeti
  O(bağlantı)dir, O(dirty × bağlantı) değildir. Kanal doluysa batch atılır ve
  sayılır (`dropped_frames`); snapshot'lar kendi kendine yettiği için bu
  yalnızca o istemciye 1 tick bayatlık olarak yansır.

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
  (MOVE_TO=1000, ENTITY_SPAWNED=1001, ENTITY_REMOVED=1002, ENTITY_STATE=1003).
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
  kullanılmıyor. Bunun yerine açık, deterministik `EntityVersion` component'i:
  her anlamlı mutasyonda `bump()`; yayın fazı `last_sent` versiyonla
  karşılaştırır. Sıfır gizli durum, tam denetlenebilir.
- `EntityId = u64` core'da ECS'sizdir; oyun crate'i `Entity::to_bits()` /
  `from_bits()` ile çevirir.

## 8. Yayın stratejisi ve ölçekleme (100k hedefi)

v1 stratejisi **tam, kendi kendine yeten dirty snapshot**:

- Her tick, versiyonu değişen her entity için `ENTITY_STATE` (tam konum +
  versiyon) yayınlanır; oda üyeliği değişimlerinde `ENTITY_SPAWNED` /
  `ENTITY_REMOVED`.
- Bağlantı başına batch + `try_send` (4.2): yavaş istemci server'ı yavaşlatmaz;
  atılan batch'in maliyeti 1 tick bayatlık.
- `Bytes` kopyasızlığı + protobuf'un ikili formu: kodlama maliyeti düşüktür.

Bu, 100k bağlantı hedefi için **doğru v1**'dir çünkü: (a) doğru ve basittir,
(b) sıralı replay/garanti gerektirmez, (c) ölçülebilir bir taban sağlar.

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
      → her bağlantının inbox'ına ConnIn::Shutdown  (spawn'lu gönderim)
      → her odaya RoomMsg::Shutdown + pacer.abort()
  → connection actor'ler çıkar → in_tx/out_tx düşer
      → reader pump: send hatası → çıkar
      → writer pump: kanal kapanır → çıkar + socket close
  → accept loop: JoinHandle.abort()   (belgelenmiş tek sert abort)
  → registry: mailbox kapanır → run() sona erer
```

Accept loop'un `JoinHandle` ile abort edilmesi v1'in bilinçli bir kısıtıdır:
listener'ı "kibarca kapatmak" için transport trait'ine kapatma yöntemi
eklemek gerekir; bu, rUDP'ye kadar ertelendi (yeni bir transport eklerken
birlikte ele alınacak).

## 10. v1 kısıtları

| Kısıt | Neden | Yol |
|---|---|---|
| Yayın = tam snapshot | Basitlik + düşmeye tolerans | delta → AOI (§8) |
| `tick_hz` oda başına sabit | Pacer basitliği | oda bazlı yapılandırma zaten var; dinamik adaptif tick gelecek |
| Accept loop abort | Trait'e close eklemek rUDP ile birlikte | §9 |
| Oda kapasitesi yok (sonsuza kadar oyuncu) | Demo oda | `RoomConfig.max_players` + doluluk yanıtı |
| Tek process | v1 kapsamı | §8.4 |
| Heartbeat → yalnızca ack (oturum zaman aşımı yok) | v1 kapsamı | registry'de son-görülme zaman damgası |

## 11. Test stratejisi

- **gsb-protocol:** frame encode/decode, bozuk çerçeve, tablo round-trip,
  bilinmeyen opcode.
- **gsb-lint:** yorum soyma (satır/blok/iç içe), satır numarası korunumu.
- **gsb-net:** gerçek loopback TCP üzerinde framing round-trip, çoklu
  frame yeniden derleme + EOF, aşırı boyutlu length-prefix reddi.
  (Testler echo-peer kullanır; pasif peer'da TCP yarı kapanışı davranış
  farkı yaratır.)
- **gsb-core:** oda actor'ü tick + oyuncu join/leave + pacer senkronizasyonu
  (saf kanal üzerinde, gerçek zamanlama ile).
- **gsb-server (e2e):** process-içi sunucu (ephemeral port) + gerçek TCP
  istemci: AUTH → JOIN → MOVE_TO → kendi entity'sinin versiyonlu snapshot'ı.
  Tüm yol tek test: pump → bağlantı actor → registry → oda → bevy world →
  hareket sistemi → yayın → writer pump.

## 12. Derleme zamanı korumaları

- `unsafe_code = "forbid"` — her crate'te.
- `gsb-lint` — select/kilit desenleri build hatası.
- `cargo clippy --workspace --all-targets` temiz.
- `edition = "2024"` (Rust 1.95).
