# gsb-kit — Takılabilir Oyun Bileşenleri (Tasarım)

**Durum: ONAYLANDI (2026-09-24) — uygulama fazları sürüyor; Faz 0 ve Faz 1a tamam (§10, "Faz 0 sonucu", "Faz 1a sonucu"; derlenen imzalar §4.5). Kararlar §12.**

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

### 4.5 Faz 1a: derlenen imzalar ve sapmalar

§4.1–§4.3'teki taslaklar koda karşı kontrol edilmişti ama
derlenmemişti. Faz 1a'da derlenen hâlleri (`kit/codec.rs`,
`kit/space.rs`, `kit/game.rs`, `kit/identity.rs`,
`kit/common/input.rs`):

```rust
pub trait RecordCodec: Send + 'static {
    type Marker: Component;
    type Query: ReadOnlyQueryData + SingleEntityQueryData + ReleaseStateQueryData;
    type Dirty: QueryFilter;
    type Wire: Clone + Eq + Debug + Send + 'static;
    fn wire(&self, item: QueryItem<'_, '_, Self::Query>) -> Self::Wire;
    fn encode(&self, id: u64, wire: &Self::Wire, out: &mut BytesMut); // yalnız gövde
}
pub trait CellSpace<W>: Send + 'static {
    type Cell: Eq + Hash + Copy + Debug + Default + Send + 'static;
    fn cell_of(&self, wire: &W) -> Self::Cell;
    fn view(&self, cell: Self::Cell) -> impl Iterator<Item = Self::Cell>;
    fn encode_cell(&self, cell: Self::Cell, out: &mut BytesMut);          // yalnız gövde
}
pub trait Game: Send + 'static {
    type Codec: RecordCodec;
    const SNAPSHOT_OP: u16 = 1003;
    const PRIVATE_OP: u16 = 1004;
    fn codec(&self) -> &Self::Codec;
    fn spawn_player(&mut self, w: &mut World, conn: ConnectionId) -> Entity;
    fn bot_actions(&mut self, _w: &World, _ctx: &TickCtx,
                   _bots: impl Iterator<Item = (PlayerId, Entity)>, _out: &mut Vec<Action>) {}
    fn ingest(&mut self, w: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>,
              players: &HashMap<PlayerId, Entity>, seq: &mut InputSeq);
    fn systems(&mut self, w: &mut World, ctx: &TickCtx);
    fn handle_request(&mut self, _w: &mut World, _ctx: &TickCtx, _r: &RpcRequest,
                      _players: &HashMap<PlayerId, Entity>) -> Option<RequestDecision> { None }
}
pub type Wire<G> = <<G as Game>::Codec as RecordCodec>::Wire;
```

Kit tarafı: `Minter` (`Sequential { used }` / `Range { base, used }`,
yalnız `kit` modülüne görünür; `mint() -> WireId`, `next_serial() ->
u64`, `used()`, `arrival(u64) -> WireId`), `InputSeq` (`admit(player,
seq)` public; `begin`/`end`/ack okuması kit-içi), `CellBook<W, C>`,
`CellPieces<C>`.

**Sapmalar ve gerekçeleri:**

1. **`RecordCodec::Query`'ye `SingleEntityQueryData +
   ReleaseStateQueryData` eklendi.** Kit bir entity'nin kaydını sorgu
   geçişi DIŞINDA da okuyor: çekirdek `on_join`'den hemen sonra, hiçbir
   `update` koşmadan `group_of`'u çağırıyor (`room/actor/control/join.rs`)
   ve AOI'nin o anki cevabı entity'nin gerçek hücresi olmalı
   (`world.entity(e).get_components::<Query>()`). İki sınır bevy'de tek
   entity okumasının şartı; `&T` ikisini de sağlıyor.
2. **`ROQueryItem` yerine `QueryItem`.** Salt-okunur bir sorguda ikisi
   aynı tip; `QueryItem` genel kodda normalize edilecek bir izdüşüm
   daha az demek.
3. **`encode` / `encode_cell` yalnız gövdeyi yazar, uzunluk metodu
   yok.** Kit zarfı `put_delimited` ile yazıyor: gövde çıktıya
   doğrudan, bir baytlık uzunluk yuvasının arkasına yazılıyor; 128
   baytı aşan (nadir) bir gövde sağa kaydırılıyor. Böylece ne
   `encoded_len` gibi gövdeyle tutarlı kalması gereken ikinci bir
   metot ne de kayıt başına bir kopya gerekiyor.
4. **`CellSpace::Cell: Default`.** `group_of` her zaman bir anahtar
   döndürmek zorunda: entity'si olmayan bir oyuncu (ya da kayıt
   bileşenleri olmayan bir entity) varsayılan hücreye düşüyor —
   `Grid2` için `Cell(0, 0)`, eski kodun cevabının ta kendisi.
5. **`Game::codec(&self)`.** Taslakta yalnız `type Codec` vardı; kodek
   metotları `&self` aldığı için odanın bir kodek DEĞERİNE ihtiyacı var
   (nicemleme parametresi taşıyan bir kodek de örnek verisidir).
6. **`bot_actions` dilim yerine `impl Iterator` alıyor.** Bugünkü
   çağrı şekli bir iteratör (park defterinin `bot` filtreli görünümü);
   dilim her tick bir `Vec` tahsisi demekti. Statik dispatch korunuyor
   (`Game` hiçbir yerde `dyn` değil).
7. **`handle_request`'e `players` eklendi.** Demo'nun işleyicisi
   isteyenin entity'sini kararlı `PlayerId` ile odanın tablosundan
   çözüyor — `ingest`'in `players` parametresiyle aynı sebep.
8. **`Mig`, `on_player_spawned`, `capture`, `restore` Faz 1a'da yok.**
   Onları kullanan odalar (takım, sharded) 1b'nin; kullanılmayan bir
   trait öğesi derlenmemiş bir taslaktan farksız. 1b ekliyor.
9. **`Minter` public bir enum değil, kit-özel.** Varyant alanları
   public olan bir enum'u herkes kurabilir, yani istediği kimliği
   basabilirdi; bu yüzden tip `pub(super)` (yalnız `kit`). İki ek:
   `next_serial` (shard'ın kararlı `PlayerId`'leri aynı aralıktan
   çekiyor — çekirdeğin tükenme bekçisi `serial_used` ikisini de
   saymalı) ve `arrival` (göçle gelen entity'nin kimliği çekirdekte ham
   `u64` taşınıyor — `Migrating::wire`, çekirdek dokunulmaz — ve alıcı
   shard onu yeniden `WireId` yapmalı; bu bir basım değil, kimliği
   kardeş shard'ın aralığı basmıştı).
10. **`CellBook<W, C>`, `CellPieces<C>`.** Taslak `CellBook<Wire>`
    diyordu; hücre anahtarı da genel (uzayın `Cell`'i). `CellPieces`
    yalnız hücre anahtarlı baytları tutuyor, `W`'ye ihtiyacı yok.

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
   bunu yapısal olarak kapatır. **Faz 1a'da kapandı:** `WireId::new`
   yok; tipin tek inşa yolu `kit/identity.rs`'teki `Minter` (üç çağrı
   noktası da ondan geçiyor), bir `compile_fail` doctest'i kilitliyor.
2. **Hayalet entity şüphesi:** AOI ve spatial kompozit despawn'ı yalnızca
   `on_leave` / göç çıkışından öğreniyor. Oyun kodunun despawn ettiği
   bir NPC büyük olasılıkla defterde ve `last_cell`'de kalıyor.
   **Doğrulanmadı** — Faz 1'de önce testle kanıtlanacak.
3. **`clear_trackers` yalnız iki odada çağrılıyor** (AOI ve spatial
   kompozit). Open, Team, PVS ve `ShardedRoom`'da silinen-bileşen
   tamponlarının büyüdüğünden şüpheleniliyor. **Ölçülmedi.**
   **Faz 1a:** `OpenRoom` için testle kanıtlandı (her `on_leave`
   despawn'ı odanın ömrü boyunca tamponda kalıyordu) ve kapandı; Team,
   PVS ve `ShardedRoom` 1b'de.
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
| 0 | `gsb-game` içinde modül bölmesi: `kit/` ve `demo/`, geçici bir ara modül; davranış değişmez | ~1 gün | tüm testler değişmeden yeşil — **tamam**, aşağıda "Faz 0 sonucu" |
| 1 | Bağımlılığın ters çevrilmesi: §4 trait'leri, generic `CellBook`/`CellPieces`, kit'e ait `WireId`/basım/Private zarfı, sharded park/join kopyalarının birleştirilmesi, §8 açıklarının testle doğrulanıp kapatılması, kit için küçük bir 2D test oyunu — iki tura bölündü: **1a tamam** (aşağıda "Faz 1a sonucu"), 1b: takım sisi, PVS, sharded kompozitler, oradaki §8 açıkları, seam'in silinmesi | ~3,5 bin satır dokunulur, +400–600 yeni | **baytlar birebir aynı** + loadgen gürültü içinde |
| 2 | Crate bölmesi: `gsb-kit` (+ kendi proto'su) ve `gsb-demo`; `gsb-server` yolları | ~400–600 satır, çoğu yol | tüm testler + loadgen |
| 3 | 3D arena demosu (`gsb-demo-arena`): bileşenler, hareket, codec, savaş sisi / takım görüşü (`Vision` + `Grid3`), proto | ~800–1 200 satır | kabul kriteri 1 (§11) |
| 4 | 3D MMO demosu (`gsb-demo-mmo`): büyük dünya, sharded × spatial, NPC'ler, park/bot (§13) | ~1 000–1 500 satır | **kapanış doğrulaması** — üç demo birlikte |

Her fazın sonunda loadgen karşılaştırması alınır (tek oda, `spatial`,
sharded). Faz 1'in en riskli parçası `CellBook`/`CellPieces`'i
sınır-şeridi entegrasyonuyla (`sharded/spatial.rs:109-169`) birlikte
generic yaparken baytları birebir korumaktır; `wire_contract.rs`,
`delta_aoi.rs` ve `aoi/tests/sharing.rs` bunu kilitler ve Faz 1 sonunda
**değişmeden** geçmek zorundadır.

### Faz 0 sonucu

**Tamamlandı** (`kit/phase-0`, `a51e8c7..`; CHANGELOG "gsb-kit Faz 0
turu"). `gsb-game/src` artık iki özel modülden oluşuyor:

- `kit/` — `room` (`OpenRoom`), `aoi`, `team`, `pvs`, `sharded`,
  `common` (hücre-delta motoru, park politikası, girdi sıra/ack kuralı
  `InputState::admit`, `Private` çerçeveleme, yetim damgalama, basım),
  `identity` (`WireId` — §4.4 gereği Faz 0'da kit'e taşındı) ve geçici
  `seam`.
- `demo/` — `components`, `systems` (+ `movement_runner`), `economy`,
  `op`, `game` (üretilen proto), `register`, `spawn` (`spawn_pos`,
  `DEFAULT_SPAWN_HALF`, `spawn_player`, `team_of`, `restore_migrant`),
  `input` (`MOVE_TO` çözme), `bot`, `rpc` (`ABILITY`/`ECONOMY`),
  `sectors` (PVS haritası), `wire` (`StripPos`).

Eski public yolların hepsi (`gsb_game::room`, `::aoi`, `::team`, `::pvs`,
`::sharded`, `::components`, `::systems`, `::economy`, `::op`, `::game`,
`::register`, `::DEFAULT_DISCONNECT_GRACE`) kökten yeniden ihraç
ediliyor; `crates/gsb-game` dışında tek bir dosya değişmedi. Bu
dokümandaki `dosya:satır` referansları Faz 0 öncesi yollardır:
`common/…` → `kit/common/…`, `aoi|team|pvs|sharded/…` → `kit/…/…`,
`room/…` → `kit/room/…`, `common/bot.rs` → `demo/bot.rs`,
`components.rs` → `demo/components.rs` + `kit/identity.rs`.

**Değişmez (kilitli):** `kit/` altında `seam.rs` dışındaki her `crate::`
yolu `kit::` ile devam eder (yorumlar dahil). Bu, "seam dışında
`crate::demo` yok" kuralından bilinçli olarak sıkıdır: kök, demo
öğelerini eski yollardan yeniden ihraç ettiği için `crate::room::spawn_pos`
yazan bir kit dosyası `crate::demo` yazmadan seam'i atlardı. Kural
`src/layering.rs`'teki kaynak-tarayan birim testiyle kilitli
(mutation-check: `kit/team/logic.rs`'e eklenen bir `crate::room`
referansı testi kırıyor). Elle: `grep -rn "crate::demo"
crates/gsb-game/src/kit` → yalnız `kit/seam.rs`.

**Seam envanteri = Faz 1 iş listesi** (25 öğe, 19 `use` satırı; ayrıca
yalnız testlerde 4 öğe):

| Hedef seam | Öğeler |
|---|---|
| `RecordCodec` (§4.1) | `Position` (Marker/Query; Vision/SectorMap/Partition'ın `Pos`'u da bu), `EntityRecord` (kayıt gövdesi), `StripPos` (`Wire` = sharded `Strip`) |
| `CellSpace` (§4.2) | `CellExit` (`encode_cell` gövdesi) |
| `Vision` (§4.2) | — (`Position` okur; ızgara zaten kit'te, gelecekteki `Grid2`) |
| `SectorMap` (§4.2) | `Sector`, `SECTOR_OUT`, `sector_of`, `VISIBLE_FROM` |
| `Partition` (§4.2) | — (`Position` okur; ızgara bölme zaten kit'te, gelecekteki `GridPartition2`) |
| `Game` kancaları (§4.3) | `spawn_player`, `team_of` (`on_player_spawned`), `MoveTarget` + `Speed` (`Mig`/capture), `restore_migrant` (`restore`), `movement_runner` (`systems`), `ingest`, `synthesize_bot_moves` (`bot_actions`), `handle_request` + `EconomyService`, `DEFAULT_SPAWN_HALF`, `WORLD_SNAPSHOT`/`PRIVATE` (`SNAPSHOT_OP`/`PRIVATE_OP`) |
| Kimlik (§4.4) | — (`WireId` kit'e taşındı; kalan iş kit-içi: `Minter`, §8.1) |
| Kit zarfı (§5) | `WorldSnapshot`, `Private`, `private::Payload`, `InputAck` (Faz 2'de kit proto'suna) |
| Test fikstürleri | `DEFAULT_SPEED`, `SECTOR_WEST`/`EAST`/`NW` (Faz 1'in kit test oyunu yerine geçer) |

**Temiz bölünmeyenler** (kit tarafında kaldı, demo bağımlılığı seam'den):

1. `collect_migrations`: yakalama (capture) bölge sorgusuyla kaynaşık
   (`(&WireId, &Position, &Speed, Option<&MoveTarget>)`); ayırmak sorgu
   sırasını/filtresini değiştirirdi. `&Speed` şartı (§8.5 açığı)
   olduğu gibi korundu.
2. `ShardedRoomState`: public tip, oyun durumunu (`pos`, `speed`,
   `target`) ve kit durumunu (`park`) birlikte taşıyor — Faz 1'de
   `KitMig<G::Mig>`.
3. Snapshot kodlayıcıları (`OpenRoom`, `TeamRoom`, `SectorRoom`,
   `ShardedRoom`, `match_result` ×2) ve `stamp_orphans`/`dirty_pass`:
   `EntityRecord`'u `Position`'dan satır içinde kuruyorlar —
   `RecordCodec`'in ta kendisi; Faz 1 işi.
4. Odaların `spawn_half` ve `economy` alanları + `with_spawn_half` /
   `with_economy` kurucuları: demo yapılandırması kit odasında duruyor,
   çünkü public kurucu imzaları Faz 0'da değişemez.
5. Sharded park kopyası (`kit/sharded/room/logic.rs` `on_disconnect` …
   `on_resume`) ortak `park.rs` ile birleştirilmedi — §4.4'e göre Faz 1.

**Tasarıma notlar (sapma değil, kayıt):**

- `in_convex`, harita verisiyle birlikte `demo/sectors.rs`'e gitti;
  §7'nin `ConvexSectors2` ön-ayarı Faz 1'de onu kit'e geri alacak.
- `OpenRoom` ve `ShardedRoom`'daki birebir aynı iki `handle_request`
  gövdesi tek bir demo fonksiyonunda (`demo/rpc.rs`) birleşti (mantık
  aynı, iki kopya → bir).
- `match_result` §4.3'teki `Game` listesinde yok: kodek + zarfla kit
  tarafında genel yazılabilir (Faz 1).
- Kök uyumluluk modülleri (`gsb_game::room`, `::pvs`, `::sharded`,
  `::components`) artık kısa belge taşıyor; stratejilerin uzun modül
  belgeleri özel `kit::*` modüllerinde (rustdoc uyarısı 37 → 14).

**Görünürlük genişletmeleri** (hiçbir struct alanı genişletilmedi):
`team_of`, `sector_of`, `VISIBLE_FROM`, `SECTOR_WEST/EAST/NW` —
private → `pub(crate)` (modül sınırını geçtikleri için);
`pub(in crate::sharded)` alanlar aynı kapsamla `pub(in
crate::kit::sharded)` oldu.

**Doğrulama:** 411 test (409 + 2 katman testi) / 0 hata / 1 ignored;
`wire_contract.rs`, `delta_aoi.rs` ve `aoi/tests/sharing*.rs`
değişmeden geçti (yalnız modül içi testlerin import yolları değişti).
Loadgen (50 istemci), 412d863 ↔ HEAD:

| Koşu | left | errors | server_closes | step_p50_fine_us | snap_total | out_bps_per_conn |
|---|---|---|---|---|---|---|
| tcp 3 sn — taban | 50 | 0 | 0 | 32 | 4100 | 11021 |
| tcp 3 sn — HEAD | 50 | 0 | 0 | 40 | 4150 | 10961 |
| udp 3 sn — taban | 50 | 0 | 0 | 40 | 4150 | 11019 |
| udp 3 sn — HEAD | 50 | 0 | 0 | 32 | 4150 | 11053 |
| sharded N=4, 8 sn — taban | 50 | 0 | 0 | 24 | 11526 | 7129 |
| sharded N=4, 8 sn — HEAD | 50 | 0 | 0 | 40 | 11470 | 7093 |

`step_p50_fine_us` 8 µs'lik kovalarla ölçülür; sharded farkı için üçer
dönüşümlü A/B koşusu alındı: taban 32/32/24, HEAD 40/24/16 — gürültü
içinde.

### Faz 1a sonucu

**Tamamlandı** (`kit/phase-1`, `ee019e8..`; CHANGELOG "gsb-kit Faz 1a
turu"). Faz 1 iki tura bölündü: 1a seam'leri kurdu, ortak makineyi ve
en basit iki stratejiyi çevirdi; 1b takım sisini, PVS'i, sharded
kompozitleri ve oradaki §8 açıklarını çevirip seam'i silecek.

- **Seam'ler:** `kit::codec::RecordCodec`, `kit::space::CellSpace<W>`
  (+ `Grid2` ön-ayarı: `Cell`, 3×3 görünüm, `cell_of`, `CellExit`
  gövdesi), `kit::game::Game`. Derlenen imzalar ve her sapma: §4.5.
- **Kimlik:** `WireId`'nin yapıcısı yok; tek inşa yolu kit-özel
  `Minter` (§8.1 kapandı).
- **Girdi:** `InputSeq` — oyunun gördüğü tek şey `admit(player, seq)`.
- **Delta motoru:** `CellBook<W, C>` / `CellPieces<C>` /
  `assemble_group_packet` generic; kayıt gövdesi `RecordCodec::encode`'dan,
  hücre-çıkış gövdesi `CellSpace::encode_cell`'den; zarfları (sequence,
  delta bayrağı, `removed`, `cell_exits`, `entities`, sabit sıra) kit
  yazıyor (`common/frame.rs`).
- **Odalar:** `OpenRoom<G: Game>`, `AoiRoom<G: Game, S:
  CellSpace<Wire<G>>>`. Ortak muhasebe `common/hooks.rs`'te kit ile
  `Game` kancaları arasında bölündü (`join`: oyun spawn eder, kit basar
  ve damgalar; `ingest`: kit bot-beslenen park oyuncularını verir, oyun
  çözer; `systems`; `close_change_window`); `stamp_orphans` kodekin
  `Marker`'ı üzerinden generic.
- **Demo:** `DemoGame` (`demo/play.rs`), `DemoCodec` (`demo/codec.rs`:
  `Marker = Position`, `Query = &Position`, `Dirty = Changed<Position>`,
  `Wire = (i32, i32)`), `Grid2` ön-ayarı. Eski kurucular
  (`OpenRoom::{new, with_spawn_half, with_economy}`,
  `AoiRoom::{new, with_spawn_half}`) `demo/rooms.rs`'te
  `OpenRoom<DemoGame>` / `AoiRoom<DemoGame, Grid2>` üzerinde;
  `gsb_game::room::OpenRoom` ve `gsb_game::aoi::AoiRoom` bu
  örneklemelere tip takma adı. `crates/gsb-game` dışında tek dosya
  değişmedi.
- **Değişiklik takibi (§4.4):** filtre kodekin (`Dirty`);
  `World::clear_trackers`'ı her iki çevrilmiş oda da `update` sonunda
  bir kez, kit'in `close_change_window`'u ile çağırıyor. Debug
  derlemelerde bir `Game` kancasının onu çağırması yakalanıyor
  (`last_change_tick` karşılaştırması). `OpenRoom` bunu hiç
  çağırmıyordu — §8.3 onun için kanıtlandı ve kapandı.

**Tasarımla çelişen / tasarımın öngörmediği (kayıt):**

1. **Seam sayıca küçülmedi.** Çevrilen iki oda seam'den hiçbir şey
   almıyor, ama kalan her öğenin 1b'de bir tüketicisi var (takım, PVS,
   sharded) ya da kit zarfı (Faz 2). Tek gerçek çıkış `CellExit`
   (artık yalnız test fikstürü); giriş: `DemoCodec` (sharded spatial
   kompozit generic motoru demo kodekiyle örnekliyor) ve test fikstürü
   `DemoGame`. Seam: 25 öğe (19 `use` satırı) + 6 test-yalnız öğe.
2. **`AoiRoom<G>` isteği oyuna yönlendirmiyor.** AOI odası hiçbir zaman
   istek cevaplamadı (çekirdek "no handler" diyordu); `Game::handle_request`'e
   bağlamak AOI'nin `ABILITY`/`ECONOMY` cevaplamaya başlaması, yani bir
   davranış değişikliği demek. Bilinçli olarak bağlanmadı; karar
   bekliyor (1b ya da kullanıcı).
3. **Faz 2 için tuzak:** demo'nun kurucuları kit'in generic tipleri
   üzerinde inherent impl (`impl OpenRoom<DemoGame> { fn new() }`).
   Bu yalnız iki taraf aynı crate'teyken yasal; crate bölmesinde
   bunlar serbest fonksiyonlara ya da bir uzantı trait'ine dönmeli
   (`gsb_game::room::OpenRoom::new()` yolu o zaman değişir).
4. **`Grid2` bir wire biçimi taşıyor:** `CellExit` gövdesini (`sint32 x
   = 1; sint32 y = 2;`, proto3 sıfır atlama dahil) kit ön-ayarı
   yazıyor; demo'nun tipli `CellExit`'ine bir demo testi sabitliyor.
   Başka bir hücre-çıkış biçimi isteyen oyun kendi `CellSpace`'ini
   yazar.
5. **`Pos2` ön-ayarı kurulmadı:** 1a odalarının kullandığı tek kodek
   demo'nunki; nicemleme (kesme) bir oyun kararı, `DemoCodec` demo'da.
6. **Katman tarayıcısı genişledi:** `pub(in crate::kit)` gibi çıplak
   `crate::kit` yolu artık kabul ediliyor (öncesinde yalnız `kit::`);
   öz-testi hem bunu hem de `crate::kitchen`'ın hâlâ yakalandığını
   sabitliyor.

**Kalan seam envanteri = Faz 1b iş listesi:**

| Hedef seam | Öğe → tüketiciler |
|---|---|
| `RecordCodec` | `Position` → takım, PVS, sharded; `EntityRecord` → takım/PVS/sharded kodlayıcıları; `StripPos` → sharded; `DemoCodec` → sharded spatial |
| `SectorMap` | `Sector`, `SECTOR_OUT`, `sector_of`, `VISIBLE_FROM` → PVS |
| `Game` kancaları | `spawn_player` → takım, PVS (`common::on_join`), sharded; `team_of` → takım; `MoveTarget` + `Speed` (`Mig`) → sharded; `restore_migrant` → sharded; `movement_runner`, `ingest`, `synthesize_bot_moves` → takım, PVS, sharded; `handle_request` + `EconomyService` → sharded; `DEFAULT_SPAWN_HALF` → takım, PVS; `WORLD_SNAPSHOT`/`PRIVATE` → takım, PVS, sharded |
| Kit zarfı (Faz 2) | `WorldSnapshot`, `Private`, `private::Payload`, `InputAck` → `common::emit_private` (her oda), takım/PVS/sharded kodlayıcıları |
| Test fikstürleri | `DemoGame`, `CellExit`, `DEFAULT_SPEED`, `SECTOR_WEST`/`EAST`/`NW` |

Faz 0'ın "temiz bölünmeyenler" listesinden kalan: `collect_migrations`
ve `ShardedRoomState` (1b), takım/PVS/sharded snapshot kodlayıcıları +
sharded `match_result` (1b — `OpenRoom`'unki ve `stamp_orphans`/
`dirty_pass` generic oldu), takım/PVS/sharded odalarındaki
`spawn_half`/`economy` alanları (çevrilen iki odada artık `DemoGame`
durumu), sharded park kopyası (1b).

**Görünürlük:** hiçbir struct alanı genişletilmedi (`ShardedRoom`'un
`serial_used` alanı aynı kapsamla `minter` oldu). Yeni public öğeler:
`InputSeq` (+ `admit`), `OpenRoom::{with_game, game, game_mut}`,
`AoiRoom::{with_game, game, game_mut}`, `Grid2`, `Cell: Default`,
`DemoGame`, `DemoCodec`, `kit::{codec, space, game}` modülleri.
Daraltılan: `common::on_join` / `stamp_orphans` → `pub(super)`.

**Doğrulama:** 419 test (411 + 8: iki `Minter`, bir `compile_fail`
doctest, `put_delimited`, iki demo wire sabitlemesi, değişiklik
penceresi, kanca bekçisi) / 0 hata / 1 ignored. `wire_contract.rs`,
`delta_aoi.rs`, `aoi/tests/sharing.rs` ve `aoi/tests/sharing/delta.rs`
değişmeden geçti (`git diff ee019e8..` boş); yalnız `aoi/tests.rs`'e
demo örneklemesini adlandıran bir tip takma adı eklendi. Loadgen (50
istemci, 3 sn), ee019e8 ↔ 4628aef dönüşümlü (sonraki commit'ler kod
değiştirmiyor):

| Koşu | left | errors | server_closes | step_p50_fine_us (3 çift) | snap_total | out_bps_per_conn |
|---|---|---|---|---|---|---|
| tcp — taban | 50 | 0 | 0 | 72 / 56 / 56 | 4100 | 11018 |
| tcp — HEAD | 50 | 0 | 0 | 88 / 48 / 56 | 4101 | 11067 |
| udp — taban | 50 | 0 | 0 | 56 / 64 / 64 | 4150 | 11048 |
| udp — HEAD | 50 | 0 | 0 | 56 / 64 / 64 | 4100 | 11004 |
| spatial — taban | 50 | 0 | 0 | 128 / 120 / 128 | 4091 | 1982 |
| spatial — HEAD | 50 | 0 | 0 | 128 / 120 / 136 | 4086 | 1934 |
| sharded N=4 — taban | 50 | 0 | 0 | 40 / 48 | 4082 | 5667 |
| sharded N=4 — HEAD | 50 | 0 | 0 | 40 / 48 | 4033 | 5792 |
| sharded × spatial — taban | 50 | 0 | 0 | 56 / 64 | 4034 | 2134 |
| sharded × spatial — HEAD | 50 | 0 | 0 | 64 / 64 | 4047 | 2164 |

snap_total / out_bps ilk çiftin değerleri. Tek çiftlik 2 kovalık fark
(tcp 72 ↔ 88) diğer çiftlerde tekrarlanmıyor — gürültü içinde.

## 11. Kabul kriteri

Tasarım, şu dört koşul sağlandığında tamamlanmış sayılır:

1. **Üç farklı demo aynı kit'i kullanır** (üç kontrol mekanizması, §12):
   mevcut 2D `sint32` demo, 3D arena ve 3D MMO, kit'in stratejilerini
   ve delta motorunu **kit koduna hiç dokunmadan** kullanır. Her demo
   ayrı bir crate'tir ve yalnız `gsb-kit`'in **public** yüzeyini
   görür — `pub(crate)` bir şeye erişemediği için kanıt yapısaldır.
   Bir demo'da çalışıp diğerinde çalışmayan her şey kit'e değil oyuna
   aittir; bir demo'nun ihtiyacı kit'te değişiklik gerektiriyorsa bu
   bir tasarım bulgusudur ve kayda geçirilir.
2. **Mevcut istemciler değişmeden çalışır:** 2D demo'nun wire baytları
   yeniden düzenleme öncesiyle birebir aynıdır.
3. **Performans:** loadgen sonuçları yeniden düzenleme öncesiyle gürültü
   içindedir.
4. **`gsb-core` dokunulmamıştır.**

## 12. Kararlar (kullanıcı, 2026-09-24)

1. **İsimler:** `gsb-kit` ve `gsb-demo` (`gsb-game` yeniden adlandırılır).
2. **§8'deki açıklar Faz 1'de** kapanır — her biri önce davranış testiyle
   kanıtlanır, sonra ayrı commit'le düzeltilir.
3. **Üç kontrol demosu** (Faz 3 ve Faz 4):

| Demo | Crate | Konum / wire | Sınadığı kit yüzeyi |
|---|---|---|---|
| 2D (mevcut) | `gsb-demo` | `Pos2` f32 sim, `(i32,i32)` wire | **bayt uyumluluğu** (mevcut istemciler, loadgen), tüm stratejiler |
| 3D arena | `gsb-demo-arena` | `Pos3<f32>`, 3D wire | **savaş sisi / takım görüşü** (MOBA tarzı: bir takım, üyelerinden herhangi birinin gördüğünü görür); `Vision` seam'i 3D mesafeyle, görüş-komşuluk ızgarası olarak `Grid3` (27 hücre); 2'den fazla takım (sabit-2 açığının kapandığını sınar); küçük oda, hızlı hareket |
| 3D MMO | `gsb-demo-mmo` | `Pos3<f32>`, yer-düzlemi hücre | **grid AOI + shard'lı dünya**: sharded × spatial kompoziti, 3D konumda **yer-düzlemi** `Partition` + `Grid2` AOI, oyun kodunun spawn/despawn ettiği NPC'ler (hayalet-entity düzeltmesini sınar), park/bot reconnect politikası, NPC göçü (`Speed`'siz entity açığını sınar) |

Arena ve MMO bilinçli olarak farklı **görünürlük modelleri** sınar
(kullanıcı kararı): arena takım bazlı görüşü (`Vision`), MMO uzamsal
ızgara + sharding'i (`CellSpace` + `Partition`). Uzay tarafında da
farklılar: arena 3D veriyi hacimsel ızgarayla (`Grid3`) işler, MMO yer
düzlemine yansıtır (`Grid2` + `GridPartition2`). Böylece ön-ayarların 3D
veriyle iki farklı kombinasyonu da sınanır.

**Kapsam dışı (bilinçli):** `gsb-server`'ın ve loadgen'in oyundan
bağımsız yapılması (§9). Sunucu ikilisi ve loadgen 2D demo'ya bağlı kalır;
3D demolar kendi crate testleriyle (oda aktörü üzerinden, gerçek
`GameLogic` yolu) doğrulanır. Faz 4 sonunda, her demonun uçtan uca
çalışabilmesi için gereken en küçük sunucu kancası ihtiyacı ayrıca
değerlendirilir.

## 13. Faz 4 — MMO demosu (kapanış doğrulaması)

Faz 3'ün (3D arena) ardından gelir. Amaç, "her şey yerine oturdu mu"
sorusunu en ağır kullanım senaryosuyla cevaplamaktır. MMO demosu kit'te
bir değişiklik gerektirirse, bu turun raporu o değişikliği ve gerekçesini
ayrıca listeler; kabul kriteri 1'e göre bu bir tasarım bulgusudur.
