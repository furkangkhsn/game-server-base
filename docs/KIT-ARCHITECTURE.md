# gsb-kit — Takılabilir Oyun Bileşenleri (Tasarım)

**Durum: TASARIM — onay bekliyor. Kod yazılmadı.**

## 1. Neden

gsb bir oyun sunucusu **motoru** olmalı: 2D ya da 3D, izometrik, TPS,
FPS, MOBA, MMO — hangi oyun olursa olsun aynı temeli kullanabilmeli.
Unity benzetmesiyle üç katman vardır:

| Unity | gsb |
|---|---|
| Motor çekirdeği | `gsb-core` (+ `gsb-ecs`, `gsb-net`, `gsb-protocol`) |
| Paketler (Netcode, Cinemachine…) | **`gsb-kit`** — bu doküman |
| Örnek projeler | **`gsb-demo`** (bugünkü `gsb-game`) |

**Bugünkü durum.** Çekirdek zaten temiz: `gsb-core`'da konum, hücre,
grid, koordinat kavramı yoktur. Sharding bile göçü ve sınır şeridini
oyuna sorar (`ShardLogic::collect_migrations`, `collect_border`;
şerit tipi oyunun `Strip`'idir). Sorun `gsb-game`'dedir: crate'in kendi
tanımı "Demo game logic", ama motorun en değerli parçaları — görünürlük
stratejileri (AOI, takım sisi, PVS), hücre bazlı delta motoru, sharded
kompozitler, park/bot politikaları — da oradadır ve **her dosyası**
demo'nun `Position` / `EntityRecord` / `MoveTo` tiplerine doğrudan
bağlıdır (~5,5 bin satır). Bir FPS geliştiricisi bugün `AoiRoom`'u
demo'nun 2D `Position`'ını ve `x,y: sint32` wire formatını benimsemeden
kullanamaz. Sorun fazla özellik değil, **yanlış yerde bağlılıktır**.

## 2. Sınır ilkesi

> **Kit, kimin neyi ne zaman göreceğine karar verir. Baytların ne
> olduğuna ve oynanışa oyun karar verir.**

- **Kit'in işi:** görünürlük (kim hangi gruptadır), gruplama, full/delta
  seçimi, hücre defteri, sharded sahiplik ve sınır, wire kimliği
  (`WireId`) basımı, girdi sıra/ack kuralı, park/resume muhasebesi,
  snapshot/Private **zarfları**.
- **Oyunun işi:** bileşen tipleri, konum tipi ve boyutu (2D/3D),
  koordinatın wire'daki biçimi (`sint32`, `f32`, milimetre `int`),
  bir entity kaydının baytları, girdi mesajları, hareket ve tüm
  oynanış sistemleri, spawn noktaları, takım ataması, harita verisi.

### Kod haritasının hipotezi düzelttiği üç yer

1. **AOI hücresi simülasyon konumundan değil, wire değerinden
   hesaplanır.** İstemci `CellExit`'i uygulamak için aynı hücreyi
   kendisi hesaplar (`common/cells.rs:44` `cell_of(x: i32, y: i32)`,
   `book.rs:198` `pos.x as i32`). Takım sisi ve PVS ise f32 simülasyon
   konumunu kullanır; sharding her ikisini (gönderme/göç: simülasyon,
   alma filtresi: wire). Yani "ilgi" tek bir konum trait'i değildir.
2. **Delta zarfının kendisi 2D'dir.** `CellExit { sint32 x, y }`. Hücrenin
   kodlanması da oyuna/uzaya devredilmelidir.
3. **İki trait yetmez.** Her strateji eksiksiz bir `GameLogic`
   uygulamasıdır: spawn, girdi, sistemler, RPC, bot, maç sonucu da
   stratejinin içindedir. Bunlar için üçüncü bir seam (`Game`) zorunludur.

## 3. Katmanlar

```
gsb-core ── gsb-ecs ── gsb-net ── gsb-protocol      (motor; dokunulmaz)
    │
gsb-kit      stratejiler + delta motoru + sharded kompozitler
    │        + park/resume + hazır ön-ayarlar; kendi kit proto'su
    │
gsb-demo     örnek oyun: konum tipini seçer, seam'leri uygular,
    │        kit'ten istediği odaları takar
gsb-server   kompozisyon kökü + loadgen (demo'ya bağlı kalır — §9)
```

Bağımlılık yönü tek yönlüdür: **kit, demo'yu asla görmez.**

## 4. Seam'ler

Hepsi statik dispatch'tir (generic); tip silme yalnızca sunucunun
fabrikasında yapılır (`gsb-server/src/boot/factories.rs` bunu bugün de
`Box<dyn RoomLogic<…>>` ile yapıyor). Generic'ler viral değildir:
fabrika imzasında yalnız ilişkili tipler (`Cell`, `Wire`, `Mig`) görünür.

### 4.1 `RecordCodec` — bir entity'nin wire kaydı

```rust
pub trait RecordCodec: Send + 'static {
    type Marker: Component;           // yayın kümesi: bu bileşeni taşıyan her entity
    type Query: ReadOnlyQueryData;    // kaydı üretmek için okunan bileşenler
    type Dirty: QueryFilter;          // "bu entity değişmiş olabilir" sinyali
    type Wire: Clone + Eq + Debug + Send + 'static; // defter birimi (nicemlenmiş değer)
    fn wire(&self, item: ROQueryItem<'_, '_, Self::Query>) -> Self::Wire;
    fn encode(&self, id: u64, w: &Self::Wire, out: &mut BytesMut); // tek kaydın gövdesi
}
```

**Kritik karar — `Wire` ilişkili tipi.** Delta motoru bugün tipli,
nicemlenmiş `(i32, i32)` tuple'larını karşılaştırır; baytlar yalnızca
gerektiğinde üretilir. Defter `CellBook<Wire>` olunca değişiklik testi
hâlâ bir değer karşılaştırmasıdır ve **kodlama sayısı bugünküyle birebir
aynı kalır**. Opak bayt isteyen bir oyun `Wire = Bytes` seçer — o bir
özel durumdur, kural değil. Aynı `Wire` sharded sınır şeridinin tipi
de olur (`Strip = Wire`; bugünkü `StripPos` tam olarak demo'nun wire
değeridir).

Demo için: `Marker = Position`, `Query = &Position`,
`Dirty = Changed<Position>`, `Wire = (i32, i32)`. Can değeri de wire'a
giren bir oyun `Dirty = Or<(Changed<Pos>, Changed<Hp>)>` yazar.

### 4.2 Uzay trait'leri — strateji başına ne gerekiyorsa o

```rust
pub trait CellSpace<W>: Send + 'static {       // AOI — WIRE değeri üzerinde
    type Cell: Eq + Hash + Copy + Debug + Send + 'static;
    fn cell_of(&self, w: &W) -> Self::Cell;
    fn view(&self, c: Self::Cell) -> impl Iterator<Item = Self::Cell>; // 2D: 3×3, 3D: 27
    fn encode_cell(&self, c: Self::Cell, out: &mut BytesMut);          // CellExit gövdesi
}
pub trait Vision: Send + 'static {             // takım sisi — simülasyon konumu
    type Pos: Component + Copy;
    type Cell: Eq + Hash + Copy;
    fn cell(&self, p: &Self::Pos, radius: f32) -> Self::Cell;
    fn neighborhood(&self, c: Self::Cell) -> impl Iterator<Item = Self::Cell>;
    fn dist2(&self, a: &Self::Pos, b: &Self::Pos) -> f32;
}
pub trait SectorMap: Send + 'static {          // PVS — statik harita verisi
    type Pos: Component;
    type Sector: Eq + Hash + Copy + Debug + Send + 'static;
    fn sector_of(&self, p: &Self::Pos) -> Self::Sector;
    fn visible_from(&self, s: Self::Sector) -> &[Self::Sector];
}
pub trait Partition<W>: Send + 'static {       // sharding — örnek (instance) verisi
    type Pos: Component;
    fn shard_count(&self) -> usize;
    fn region_of(&self, p: &Self::Pos) -> usize;
    fn neighbors(&self, idx: usize) -> Vec<usize>;
    fn exports(&self, idx: usize, p: &Self::Pos) -> bool; // gönderen: sınıra yakın mı
    fn admits(&self, idx: usize, w: &W) -> bool;          // alan: çerçeve filtresi
}
```

`Partition` konum bileşeninin bir özelliği değildir: harita boyutu,
shard sayısı ve sınır genişliği gibi **örnek** verisi taşır ve iki konum
türünü birden okur. Bu yüzden ayrı bir değerdir.

### 4.3 `Game` — oynanış kancaları

```rust
pub trait Game: Send + 'static {
    type Codec: RecordCodec;
    type Mig: Debug + Send + 'static;             // shard göçünde taşınan oyun durumu
    const SNAPSHOT_OP: u16 = 1003;
    const PRIVATE_OP: u16 = 1004;
    fn spawn_player(&mut self, w: &mut World, conn: ConnectionId) -> Entity; // WireId'yi kit basar
    fn on_player_spawned(&mut self, _w: &mut World, _c: ConnectionId, _e: Entity) {}
    fn ingest(&mut self, w: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>,
              players: &HashMap<PlayerId, Entity>, seq: &mut InputSeq);
    fn bot_actions(&mut self, _w: &World, _ctx: &TickCtx,
                   _bots: &[(PlayerId, Entity)], _out: &mut Vec<Action>) {}
    fn systems(&mut self, w: &mut World, ctx: &TickCtx);
    fn handle_request(&mut self, _w: &mut World, _ctx: &TickCtx, _r: &RpcRequest)
        -> Option<RequestDecision> { None }
    fn capture(&self, w: &World, e: Entity) -> Self::Mig;
    fn restore(&mut self, e: EntityWorldMut<'_>, m: Self::Mig);
}
```

Oda tipleri bunları bir araya getirir:

| Oda | `GroupKey` | `Strip` | Shard durumu |
|---|---|---|---|
| `OpenRoom<G>` | `()` | `()` | — |
| `AoiRoom<G, S: CellSpace<Wire<G>>>` | `S::Cell` | `()` | — |
| `TeamRoom<G, V: Vision>` | `TeamId` | `()` | — |
| `SectorRoom<G, M: SectorMap>` | `M::Sector` | `()` | — |
| `ShardedRoom<G, P: Partition<Wire<G>>>` | `()` | `Wire<G>` | `KitMig<G::Mig>` |
| `ShardedSpatialRoom<G, P, S>` | `S::Cell` | `Wire<G>` | `KitMig<G::Mig>` |

Bu sınırların hepsi `GameLogic` / `ShardLogic`'in bugünkü
kısıtlarıyla (`GroupKey: Eq + Hash + Clone + Debug`,
`Strip: Debug + Clone + PartialEq + Send + 'static`) karşılanabilir.

### 4.4 Kit'in kendisinin sahip olduğu şeyler

- **Kimlik:** `WireId(u64)` ve basımı kit'e taşınır, yapıcı kit-özel
  kalır (`Minter::Sequential` / `Minter::Range`). Oyun bir entity'yi
  "yayınlanabilir" yapmak için yalnızca `Marker` bileşenini ekler; kimliği
  kit damgalar.
- **Değişiklik takibi:** filtre oyunundur (`Dirty`), ama
  `World::clear_trackers()`'ı tick başına **tek bir kez** kit çağırır.
  Bu dünya-geneli bir çağrıdır; iki sahibi olamaz.
- **Silinme:** oyun kodunun despawn ettiği entity'ler (mermi, NPC) kit
  tarafından `world.removed::<WireId>()` ile öğrenilir (§8'deki açığa
  bkz.).
- **Girdi sıra/ack kuralı:** yüksek-su kuralı ve `InputAck` kit'tedir;
  girdinin çözülmesi ve uygulanması oyunun `ingest`'indedir.
- **Park/resume:** `common/park.rs` zaten oyundan bağımsızdır, olduğu
  gibi taşınır; sharded yoldaki kopyası (`sharded/room/logic.rs:166-234`)
  onunla birleştirilir. `bot.rs` ise tamamen demo'dur; kit yalnızca
  "bot beslenen park oyuncuları" listesini `Game::bot_actions`'a verir.

## 5. Wire

Kit kendi proto'sunu taşır (`gsb.kit`):

```proto
message WorldSnapshot {
  uint64 sequence = 1;
  repeated bytes entities = 2;    // oyunun RecordCodec::encode çıktısı
  repeated uint64 removed = 3;
  repeated bytes cell_exits = 4;  // CellSpace::encode_cell çıktısı
  bool delta = 5;
}
```

Protobuf'ta `repeated EntityRecord` ile aynı numaradaki `repeated bytes`
**birebir aynı baytları** üretir (ikisi de length-delimited). Bu yüzden
demo'nun `game.proto`'su istemci tarafı için tipli aynasını
(`EntityRecord`, `CellExit`) koruyabilir ve **mevcut istemciler hiç
değişmeden çalışır**. İki tanımı aynı baytlara sabitleyen bir
wire-sözleşme testi zorunludur.

Kit, zarfları bugün de elle yazıyor (`cells.rs:54-103`,
`aoi/logic.rs:154`, `common/mod.rs:267`); değişen tek şey kayıt ve
hücre gövdelerinin oyundan gelmesidir. `Private` bugün kapalı bir
oneof'tur; oyunun kendi özel yükünü taşıyabilmesi için ayrılmış bir
`bytes` alanı eklenmelidir.

## 6. Hareket bir trait değildir

Hareket oynanışın kendisidir: FPS'te fizik, zıplama, eğilme; MOBA'da
tıkla-git ve yol bulma; izometrik oyunda grid. Hepsini kapsayan bir trait
ya o kadar ince olur ki `gsb-ecs`'teki `System` trait'inden farkı kalmaz,
ya da tek bir oyun türüne göre kesilir. Oyunun sistemleri `Game::systems`
ile çalışır; kit yalnızca **isteğe bağlı** hazır yardımcılar sunar (§7).

## 7. Hazır ön-ayarlar

Oyun yazarının her şeyi sıfırdan uygulamaması için kit, trait'lerin
yaygın durumlar için hazır uygulamalarını taşır:

| Ön-ayar | İçerik |
|---|---|
| `Pos2<i32>`, `Pos2<f32>`, `Pos3<f32>` | konum bileşenleri + `RecordCodec` (farklı nicemleme seçenekleriyle) |
| `Grid2`, `Grid3` | `CellSpace` + `Vision` (2D'de 3×3, 3D'de 27 hücre görünüm) |
| `ConvexSectors2` | 2D dışbükey çokgen sektörlerle `SectorMap` |
| `GridPartition2`, `GridPartition3` | ızgara `Partition` (bugünkü 2D bölme bunun ilk örneği) |
| `KinematicMover<P>` | isteğe bağlı "hedefe doğru ilerle" sistemi, 2D/3D |

İzometrik bir oyun `Pos2<f32>` + `Grid2` takar; bir FPS `Pos3<f32>`
seçer ve AOI'yi yer düzleminde (`Grid2`) ya da hacimsel (`Grid3`)
yapabilir. Alışılmadık bir oyun kendi tipine trait'leri uygular.

## 8. Harita sırasında bulunan mevcut açıklar

Bunlar yeniden düzenlemeden bağımsız, bugün de var olan sorunlardır.
Faz 1'de davranış testleriyle doğrulanıp ayrı commit'lerle kapatılır:

1. **Bayat değişmez:** DESIGN "`common::next_serial`, `WireId::new`'ün
   tek çağrıcısı" diyor; oysa `sharded/room/logic.rs:120,268` ve
   `sharded/room/shard.rs:88` de çağırıyor. Kit'e taşınan `Minter`
   bunu yapısal olarak kapatır.
2. **Hayalet entity şüphesi:** AOI ve spatial kompozit despawn'ı yalnızca
   `on_leave` / göç çıkışından öğreniyor. Oyun kodunun despawn ettiği
   bir NPC büyük olasılıkla defterde ve `last_cell`'de kalıyor.
   **Doğrulanmadı** — Faz 1'de önce testle kanıtlanacak.
3. **`clear_trackers` yalnız iki odada çağrılıyor** (AOI ve spatial
   kompozit). Open, Team, PVS ve `ShardedRoom`'da silinen-bileşen
   tamponlarının büyüdüğünden şüpheleniliyor. **Ölçülmedi.**
4. **Sabit sınırlar:** takım sayısı 2'ye sabit (`[_; 2]` diziler), PVS
   görünürlük tablosu `u16` bitmask olduğu için 16 sektörle sınırlı,
   bitişik olmayan bir bölgeye düşen entity hiçbir komşuya göç
   ettirilmiyor.
5. **Varlık tipine bağlı göç:** `Speed` bileşeni olmayan bir NPC bugün
   hiç göç etmiyor (`sharded/room/shard.rs:50` sorgusu `&Speed` istiyor).

## 9. Elenen alternatifler

- **`MovementSystem` trait'i.** §6.
- **`Visibility` trait'i** (DESIGN §8'de zaten elenmişti): stratejilerin
  içerik hesapları ortak bir imza paylaşmaz. Bu tasarım o kararı geri
  almıyor: burada soyutlanan şey görünürlük değil, **oyunun kendisi**
  (codec, uzay, harita). Stratejiler ayrı oda tipleri olarak kalır.
- **Konumu doğrudan trait yapmak** (`trait Interest` konum bileşeninde):
  AOI wire değerine, takım sisi simülasyon konumuna, sharding ikisine
  birden bakıyor; tek bir konum trait'i bu üç ihtiyacı karşılayamaz.
- **Her zaman opak bayt karşılaştırmak:** değişip değişmediğini anlamak
  için her kirli entity'yi her tick kodlamak gerekir; tahmini
  tick başına +0,3–0,6 M ek kodlama, ek olarak değişen her kayıt için
  bir `Bytes` tahsisi. `Wire` ilişkili tipi aynı esnekliği bu bedel
  olmadan verir.
- **u64 parmak izi ile karşılaştırma:** çakışma, bir güncellemeyi
  sessizce düşürür.
- **Kit'e ait bir `Dirty` işaret bileşeni:** tasarımın bilinçli olarak
  kaldırdığı yazar disiplinini (her sistem işareti hatırlamak zorunda)
  geri getirir (`aoi/mod.rs:90-108`).
- **Motor-alanı deseni** (oyun `GameLogic`'i kendisi uygular, kit'in
  `AoiEngine`'ine delege eder): oyunu `on_leave`, `update`, göç
  çağrılarını hatırlamakla yükümlü kılar; aynı disiplin sorunu.
- **Önce crate'leri bölmek:** stratejiler bugün demo kodunu çağırıyor
  (`movement_runner`, `ingest`, `synthesize_bot_moves`,
  `handle_request`, `spawn_pos`). Önce bölmek kit'i demo'ya bağımlı
  yapar, yani tersi yönde. Bölme, bağımlılık ters çevrildikten
  **sonra** gelir.
- **`gsb-server`'ı şimdi oyundan bağımsız yapmak:** görünürlük ekseni ve
  loadgen demo'ya bağlıdır; sunucuyu bir "oyun modülü" üzerinden
  generic yapmak ayrı bir iştir, bu tasarımın kapsamı dışındadır.

## 10. Fazlar

| Faz | Kapsam | Büyüklük | Kapı |
|---|---|---|---|
| 0 | `gsb-game` içinde modül bölmesi: `kit/` ve `demo/`, geçici bir ara modül; davranış değişmez | ~1 gün | tüm testler değişmeden yeşil |
| 1 | Bağımlılığın ters çevrilmesi: §4 trait'leri, generic `CellBook`/`CellPieces`, kit'e ait `WireId`/basım/Private zarfı, sharded park/join kopyalarının birleştirilmesi, §8 açıklarının testle doğrulanıp kapatılması, kit için küçük bir 2D test oyunu | ~3,5 bin satır dokunulur, +400–600 yeni | **baytlar birebir aynı** + loadgen gürültü içinde |
| 2 | Crate bölmesi: `gsb-kit` (+ kendi proto'su) ve `gsb-demo`; `gsb-server` yolları | ~400–600 satır, çoğu yol | tüm testler + loadgen |
| 3 | İkinci demo: 3D `f32` (bileşenler, hareket, codec, 3D ön-ayarlar, proto, istemci görünümü) | ~800–1 200 satır | **kabul testi** (§11) |

Her fazın sonunda loadgen karşılaştırması alınır (tek oda, `spatial`,
sharded). Faz 1'in en riskli parçası `CellBook`/`CellPieces`'i
sınır-şeridi entegrasyonuyla (`sharded/spatial.rs:109-169`) birlikte
generic yaparken baytları birebir korumaktır; `wire_contract.rs`,
`delta_aoi.rs` ve `aoi/tests/sharing.rs` bunu kilitler ve Faz 1 sonunda
**değişmeden** geçmek zorundadır.

## 11. Kabul kriteri

Tasarım, şu dört koşul sağlandığında tamamlanmış sayılır:

1. **İki farklı demo aynı kit'i kullanır:** mevcut 2D `sint32` demo ve
   yeni 3D `f32` demo, AOI'yi ve delta motorunu **kit koduna hiç
   dokunmadan** kullanır. Birinde çalışıp diğerinde çalışmayan her şey
   kit'e değil oyuna aittir.
2. **Mevcut istemciler değişmeden çalışır:** 2D demo'nun wire baytları
   yeniden düzenleme öncesiyle birebir aynıdır.
3. **Performans:** loadgen sonuçları yeniden düzenleme öncesiyle gürültü
   içindedir.
4. **`gsb-core` dokunulmamıştır.**

## 12. Açık sorular (kullanıcı kararı)

1. **İkinci demo hangi tür olsun?** Öneri: küçük bir 3D `f32` "arena"
   (FPS/TPS benzeri): serbest 3D hareket + AOI + takım sisi. 2D demo'dan
   en uzak düşen örnek, soyutlamayı en sert sınayandır.
2. **İsimler:** `gsb-kit` mi, `gsb-extras` mı? `gsb-game` → `gsb-demo`
   yeniden adlandırması uygun mu?
3. **§8'deki açıklar** Faz 1'in içinde mi kapansın (öneri: evet, ayrı
   commit'lerle, çünkü taşınan kod zaten o dosyalar), yoksa önce ayrı
   bir turda mı?
