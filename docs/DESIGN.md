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
Aktörler mailbox'ına gelen mesajı bekler; pump görevleri birer stream
öğesi/kanal mesajı bekler; pacer'lar birer timer bekler. Birkaç görev
(ör. connection actor'ün join'inde) oneshot yanıtını da bekler — yine de
tek bekleme, select değil.

Bu kural üç somut biçimde uygulanır:

1. **Hiçbir `tokio::select!` yok.** Çoklu bekleme ihtiyacı, beklenecek her
   kaynak için **ayrı bir görev** açılarak çözülür (pump görevleri, pacer
   görevi, bağlantı dispatcher'ları). "Birden fazla kaynağı tek görevde
   multiplex etme" deseni bu mimaride var olmaktan çıkar; bu, büyük
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
```

- **Bağlantı başına 3 görev:** reader pump (socket → `ConnIn::Frame`),
  connection actor (durum makinesi: auth → join → forward), writer pump
  (`FrameBatch` → socket). Pump görevlerinin her biri de tek kaynaktan
  bekler: stream'in bir öğesi ya da kanal mesajı.
- **Oda başına 2 görev:** pacer (sabit tempoda `RoomMsg::Tick`; sapma
  düzeltmeli — saati yakalayamazsa tick atlar) + room actor.
- **Registry:** sunucunun kontrol düzlemi. Oda tablosu
  (`RoomId → mailbox + pacer`), bağlantı tablosu
  (`ConnectionId → {room?, entity?, inbox?}` — kayıt, bağlantının **bütün
  ömründe** yaşar; inbox asla bırakılmaz ki `RoomGone`/`Shutdown` her zaman
  ulaşabilsin) ve oyun mantığını core'e sokan `RoomFactory<W>` kapağı.
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
- **Tick birleştirme (coalescing):** tick çalışırken pacer daha çok Tick
  biriktirdiyse, actor kuyruğu sırayla boşaltır: saf Tick'ler atlanır
  (çalışan tick, dt'si üzerinden geçen toplam zamanı zaten kapsar), diğer
  mesajlar sırasına uygun biçimde hemen işlenir. Böylece yavaş bir tick
  çöp iş birikimi yaratmaz.
- `OutSink` yayın fazında bağlantı başına bir `Vec<FrameBody>` tamponlar ve
  tick sonunda **bağlantı başına tek `try_send`** yapar. Maliyet, dürüstçe:
  *kanal gönderimi* O(bağlantı)/tick, *frame tamponlaması* ise
  O(dirty × bağlantı) `Bytes` (Arc) kopyasıdır — kopya ucuzdur (payload
  asla kopyalanmaz) ama sıfır da değildir; 100k hedefinde asıl duvar bu
  çarpımdır ve AOI onu ortadan kaldırır (§8). Kanal doluysa batch atılır ve
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
  her anlamlı mutasyonda `bump()`; yayın fazı, **bağlantı başına tutulan**
  `last_sent` haritasıyla karşılaştırır (entity → son gönderilen versiyon).
  Sonradan giren bağlantının haritası boştur → bir sonraki yayında **tüm
  dünya** gönderilir (tam snapshot catch-up). AOI'ye geçişte bu yapı
  doğrudan genişletilir. Sıfır gizli durum, tam denetlenebilir.
- `EntityId = u64` core'da ECS'sizdir; oyun crate'i `Entity::to_bits()` /
  `from_bits()` ile çevirir.

## 8. Yayın stratejisi ve ölçekleme (100k hedefi)

v1 stratejisi **tam, kendi kendine yeten dirty snapshot, bağlantı başına**:

- Bir entity, *o bağlantının kaydettiği* versiyondan farklıysa o bağlantıya
  `ENTITY_STATE` (tam konum + versiyon) gönderilir. Sonradan giren bağlantı
  ilk tick'te dünyadaki **tüm** entity'leri alır. Oda üyeliği değişimlerinde
  `ENTITY_SPAWNED` / `ENTITY_REMOVED`.
- Kodlama O(dirty entity)/tick; teslim O(dirty × bağlantı) Arc kopyası.
- Bağlantı başına batch + `try_send`: yavaş istemci server'ı yavaşlatmaz;
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
      → her dispatcher'a RoomOp::Close (yol açma + son leave), sonra senders düşer
      → her bağlantının inbox'ına ConnIn::Shutdown  (spawn'lu gönderim)
      → her odaya RoomMsg::Shutdown + pacer.abort()
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
| Yayın = tam snapshot (bağlantı başına) | Basitlik + düşmeye tolerans | delta → AOI (§8) |
| `tick_hz` oda başına sabit | Pacer basitliği | oda bazlı yapılandırma zaten var; dinamik adaptif tick gelecek |
| Accept loop abort | Trait'e close eklemek rUDP ile birlikte | §9 |
| Oda kapasitesi yok (sonsuza kadar oyuncu) | Demo oda | `RoomConfig.max_players` + doluluk yanıtı |
| Tick ve Action aynı bounded mailbox'ta | Sadelik | veri düzlemi (Action) ile kontrol düzlemi (Tick) ayrılmalı |
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
- **gsb-core:** oda actor'ü tick + oyuncu join/leave + pacer senkronizasyonu
  (saf kanal üzerinde, gerçek zamanlama ile). Registry: join→leave→rejoin
  dizisi (gözlemci bağlantı üzerinden oyuncu sayısı doğrulanır — stale
  leave sayacı geri düşürmemeli), oda imhası bildirimi + imha sonrası join
  reddi, temiz shutdown (registry handle'ı çözülür).
- **gsb-game:** gecikmeli giriş — hareketsiz A'nın olduğu odaya B girerse B,
  bir tick sonra **A dahil tüm dünyayı** görür; A hareket edince B
  versiyon artışı görür. Stale leave — rejoin'dan gecikmeyle gelen eski
  `PlayerLeft`, yeni entity'yi öldüremez (hedefli MOVE_TO + snapshot ile
  doğrulanır).
- **gsb-server (e2e):** process-içi sunucu (ephemeral port) + gerçek TCP
  istemci: AUTH → JOIN → MOVE_TO → kendi entity'sinin versiyonlu snapshot'ı.
  Tüm yol tek test: pump → bağlantı actor → registry → dispatcher → oda →
  bevy world → hareket sistemi → yayın → writer pump.

## 12. Derleme zamanı korumaları

- `unsafe_code = "forbid"` — her crate'te.
- `gsb-lint` — 6 crate'in `build.rs`'inde (lint crate'i hariç) select/kilit
  desenleri build hatası; kapsam `src/` + `tests/` + `examples/`.
- `cargo clippy --workspace --all-targets` temiz.
- `edition = "2024"` (Rust 1.95).
