# gsb-kit — Takılabilir Oyun Bileşenleri (Tasarım)

**Durum: ONAYLANDI (2026-09-24) — bütün fazlar tamam; Faz 0, Faz 1 (1a + 1b), Faz 2 (crate bölmesi), Faz 3 (3D arena demosu) ve Faz 4 (3D MMO demosu, kapanış doğrulaması — ikisi de kit'e ve çekirdeğe dokunmadan), Faz 5 (kit düzeltme turu: iki demonun kaydettiği bulguların hepsi kapandı — F1 doğruluk hatası dahil) (§10, "Faz 0 sonucu" … "Faz 5 sonucu"; derlenen imzalar §4.5, §4.6 + "Faz 5 eklemeleri"; kit proto'su §5). Üç demo, düzeltilmiş kit üzerinde yeşil; kabul kriterinin dördü de sağlandı (§11). Kararlar §12.**

## 1. Neden

gsb bir oyun sunucusu **motoru** olmalı: 2D ya da 3D, izometrik, TPS,
FPS, MOBA, MMO — hangi oyun olursa olsun aynı temeli kullanabilmeli.
Unity benzetmesiyle üç katman vardır:

| Unity | gsb |
|---|---|
| Motor çekirdeği | `gsb-core` (+ `gsb-ecs`, `gsb-net`, `gsb-protocol`) |
| Paketler (Netcode, Cinemachine…) | **`gsb-kit`** — bu doküman |
| Örnek projeler | **`gsb-demo`** (eski `gsb-game`; Faz 2) |

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

Hepsi statik dispatch'tir (generic); tip silme sunucu tarafında yapılır.
*Düzeltme (GAME-MODULE §3):* "fabrikada" demek kesin değildi — fabrika
imzası `G`, `St`, `Sp`'yi `Registry<W, G, St, Sp>`'ye taşır; gerçek
generic-olmayan sınır registry'nin başlatıldığı yer
(`Mailbox<RegistryMsg>`, `gsb-server/src/game.rs` `RegistryParts::spawn`).
Demo'nun fabrikaları bugün `gsb-server/src/games/demo/factories.rs`'te. Generic'ler viral değildir:
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
    fn spawn_player_as(&mut self, w: &mut World, conn: ConnectionId, identity: &str)
        -> Entity { self.spawn_player(w, conn) }  // K4: doğrulanmış kimlikle
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

*K4 (GAME-MODULE "K4 — oyuncu kimliği → ev shard'ı"):*
`spawn_player_as` oyuncunun **doğrulanmış kimliğini** alır (ticket'ın
`player`'ı; ticket'sız geliştirme yolunda iddia edilen `Auth.name`; boş
= anonim) ve kayıtlı bir karakteri yerleştirmek için ezilir; varsayılanı
`spawn_player`. Kit odaları onu çekirdeğin `GameLogic::on_join_as`'ından
çağırır (`OpenRoom`, `AoiRoom`, `SectorRoom`, `ShardedRoom`,
`ShardedSpatialRoom`); sharded odanın yönlendiricisi
(`registry::HomeShard`) aynı kimliği görür, yani ikisi anlaşabilir.
Takım odaları (`TeamRoom`, `ShardedTeamRoom`) onun takım karşılığını
çağırır: `TeamGame::spawn_team_player_as(world, conn, identity) ->
(Entity, Team)`, varsayılanı `spawn_team_player` (W1 — K4'ün kalıntısı
kapandı; §10 "W1 sonucu").

Oda tipleri bunları bir araya getirir:

| Oda | `GroupKey` | `Strip` | Shard durumu |
|---|---|---|---|
| `OpenRoom<G>` | `()` | `()` | — |
| `AoiRoom<G, S: CellSpace<Wire<G>>>` | `S::Cell` | `()` | — |
| `TeamRoom<G, V: Vision>` | `TeamId` | `()` | — |
| `SectorRoom<G, M: SectorMap>` | `M::Sector` | `()` | — |
| `ShardedRoom<G, P: Partition<Wire<G>>>` | `()` | `Wire<G>` | `KitMig<G::Mig>` |
| `ShardedSpatialRoom<G, P, S>` | `S::Cell` | `Wire<G>` | `KitMig<G::Mig>` |
| `ShardedTeamRoom<G, P, V: Vision>` | `Team` | `Wire<G>` | `TeamMig<G::Mig>` |

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
  girdinin çözülmesi ve uygulanması oyunun `ingest`'indedir. Sharded
  odalarda oyuncunun oturum durumu (`hwm`, `acked`) göçle birlikte
  `KitMig` içinde taşınır (G2 bulguları K1–K3; `4d83d01`).
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

### 4.6 Faz 1b: kalan seam'ler, konum erişimcisi ve sapmalar

Faz 1b'de derlenen hâller (`kit/space.rs` + `kit/space/{vision,
sectors,partition}.rs`, `kit/game.rs`, `kit/sharded/mod.rs`):

```rust
pub trait Planar {                                  // konum erişimcisi (aşağıda)
    type Coord: Copy;
    fn planar(&self) -> [Self::Coord; 2];
}
pub trait Vision: Send + 'static {                  // takım sisi — simülasyon konumu
    type Pos: Component + Copy;
    type Cell: Eq + Hash + Copy + Send + 'static;
    fn cell(&self, pos: &Self::Pos) -> Self::Cell;
    fn neighborhood(&self, cell: Self::Cell) -> impl Iterator<Item = Self::Cell>;
    fn sees(&self, viewer: &Self::Pos, target: &Self::Pos) -> bool;
}
pub trait SectorMap: Send + 'static {               // PVS — statik harita verisi
    type Pos: Component;
    type Sector: Eq + Hash + Copy + Debug + Send + 'static;
    fn sector_of(&self, pos: &Self::Pos) -> Self::Sector;
    fn outside(&self) -> Self::Sector;
    fn visible_from(&self, sector: Self::Sector) -> impl Iterator<Item = Self::Sector> + '_;
}
pub trait Partition<W>: Send + 'static {            // sharding — örnek verisi
    type Pos: Component;
    fn shard_count(&self) -> usize;
    fn region_of(&self, pos: &Self::Pos) -> usize;
    fn neighbors(&self, idx: usize) -> Vec<usize>;
    fn exports(&self, idx: usize, pos: &Self::Pos) -> bool;
    fn admits(&self, idx: usize, wire: &W) -> bool;
}
pub trait TeamGame: Game {
    fn team_of(&mut self, world: &World, conn: ConnectionId, entity: Entity) -> Team;
}
// (`Game` K4'te `spawn_player_as(world, conn, identity)` kazandı — §4.3;
//  takım odası onu çağırmaz, `ShardGame` değişmedi)
pub trait ShardGame: Game {
    type Mig: Debug + Send + 'static;
    fn capture(&self, world: &World, entity: Entity) -> Self::Mig;
    fn restore(&mut self, world: &mut World, mig: Self::Mig) -> Entity;
}
pub struct KitMig<M> { pub game: M, pub park: Option<ShardParkRecord>,
                      pub input: Option<ShardInputRecord> } // Deref<Target = M>
// `input` (hwm, acked): G2 bulguları K1–K3'ün düzeltmesi, `4d83d01`
// (GAME-MODULE §5 "Kit düzeltme turu")
```

Odalar: `TeamRoom<G: TeamGame, V: Vision>` (`with_game(game, vision)`;
T turundan beri isteğe bağlı `.with_delta()` — §10 "T sonucu"),
`SectorRoom<G: Game, M: SectorMap>` (`with_game(game, map)`),
`ShardedRoom<G: ShardGame, P: Partition<Wire<G>>>` (`with_game(game,
partition, index)`; `State = KitMig<G::Mig>`, `Strip = Wire<G>`),
`ShardedSpatialRoom<G, P, S: CellSpace<Wire<G>>>` (`with_shard(inner,
space)`). Ön-ayarlar: `Grid2` (artık `CellSpace<W>` for `W: Planar<Coord
= i32>`), `VisionGrid2<P>`, `ConvexSectors2<P>` (`Sector(pub u8)`),
`GridPartition2<P>` (+ `grid_shape`, `shard_at`).

**Konum erişimcisi (`Planar`) — tasarım.** Kit oyunun `Position`'ını ve
wire tipini göremez; 2D ön-ayarların hepsi oyunun tiplerini tek bir
küçük trait üzerinden okur: `Planar { type Coord; fn planar(&self) ->
[Coord; 2] }`. Simülasyon ön-ayarları (`VisionGrid2`,
`ConvexSectors2`, `GridPartition2`'nin bölge/ihraç testi) `Coord =
f32`, wire ön-ayarları (`Grid2`, `GridPartition2::admits`) `Coord =
i32` ister. İzdüşüm **oyunun** kararıdır ve tip başına bir kez
verilir: 2D demo `[x, y]` der; Faz 4'ün MMO'su `Pos3 { x, y, z }`'si ve
3D wire tipi için `[x, z]` der ve `Grid2` AOI'yi ve `GridPartition2`
sharding'i **kit'e dokunmadan** kullanır (`kit/space/tests.rs` bunu
kit'in hiç görmediği bir `Pos3`/`Wire3` çiftiyle kilitliyor: yükseklik
hücreyi, bölgeyi, görüşü ve sektörü değiştirmiyor). Faz 2'nin 3D
ön-ayarları (`Grid3`, 3D görüş ızgarası, `GridPartition3`) ayrı bir
uzamsal erişimciyi okuyacak (`Spatial { type Coord; fn spatial(&self)
-> [Coord; 3] }`); bir tip ikisini birden uygulayabilir (arena: `Grid3`
için `Spatial`; MMO: yer düzlemi için `Planar`). *(Faz 2: `Spatial`
tam bu imzayla ve yalnız arenanın gerektirdiği 3D ön-ayar —
`VisionGrid3` — kuruldu; `Grid3` ve `GridPartition3`'ün hiçbir kontrol
demosu kullanmıyor, tetikleyici bekliyorlar: §7.)*

Elenen alternatifler:
- *Ön-ayara izdüşüm tip parametresi* (`Grid2<Proj>`, `Proj:
  Project<W>`): yabancı tipler için de çalışır (yetim kuralı), ama her
  ön-ayar ve her oda tipi bir parametre daha taşır; üç demo da kendi
  tiplerine sahip, yetim durumu doğmuyor. Gerekirse sonradan eklenir.
- *Ön-ayarda closure / fn işaretçisi* (`Grid2::new(size, |w| …)`):
  işaretçi, sıcak yolda entity başına dolaylı çağrı (statik dispatch
  kuralı); generic closure ön-ayar tipini fabrika takma adında
  adlandırılamaz yapar.
- *Kit'e ait somut konum tipi* (`Pos2`/`Pos3` bileşeni): oyunun bileşen
  ve wire tiplerini dayatır — kit'in kaldırdığı bağlılığın ta kendisi
  (§1). Kolaylık ön-ayarı olarak sonra eklenebilir.
- *`Planar<T>` (koordinat tipi generic parametre):* bir tip hem `f32`
  hem `i32` izdüşümü verebilirdi; hiç gerekmiyor (bir değer ya
  simülasyon ya wire'dır) ve kullanım yerinde sınırı belirsizleştirir.
  İlişkili tip = tip başına tek izdüşüm.
- *`Into<[f32; 2]>`:* hangi eksenlerin düzlem olduğunu ön-ayar başına
  seçemez, yabancı tipler için yine yetim kuralına takılır, ve
  "dönüştürülebilir" ile "düzlemde şurada" anlamlarını karıştırır.

**Sapmalar ve gerekçeleri** (§4.2/§4.3 taslağına ve §4.5'e göre):

1. **`Vision` yarıçapı değerin kendisi taşır; `dist2` yerine `sees`.**
   Taslak `cell(p, radius)` + `dist2(a, b)` diyordu (oda `≤ r²`'yi
   hesaplardı). Yarıçap, `CellSpace`'in hücre boyu gibi modelin örnek
   verisi; `sees` bir modelin testi kendisi tanımlamasına izin veriyor
   (3D mesafe, görüş hattı). Izgara sözleşmesi yazılı: `sees(v, t)` ⇒
   `cell(v)` ∈ `neighborhood(cell(t))`.
2. **`SectorMap::visible_from` dilim değil iteratör; `outside()`
   eklendi.** İteratör önayarın gösterimini serbest bırakıyor (1b'nin
   ilk commit'i bit maskesini birebir taşıdı, ikincisi listeye
   çevirdi — imza değişmedi). `outside()`: `group_of` her zaman cevap
   vermeli; entity'si olmayan oyuncu ve harita konumu olmayan entity
   (yalnız `Marker`'ı olan) sınırlama sektörüne düşer — hiçbir harita
   sektörüne sızmayan tek yer.
3. **`Partition`'a yönlendirme metodu eklenmedi.** Bitişik olmayan göç
   (§8.4) `neighbors()` grafı üzerinde kit'in BFS'iyle (`first_hops`)
   yönlendiriliyor; sözleşme: komşuluk simetrik, graf bağlantılı.
4. **`Mig` / `capture` / `restore` `Game`'de değil, `ShardGame:
   Game`'de.** Kararlı Rust'ta ilişkili tip varsayılanı yok: `Game`'in
   içinde, hiç shard'lanmayan her oyun (Faz 3'ün arenası) bir `Mig`
   adlandırıp hiçbir şeyin çağırmadığı iki kanca yazmak zorunda
   kalırdı. Yalnız sharded odalar `ShardGame` ister.
5. **`restore(&mut self, world, mig) -> Entity`**, taslaktaki
   `restore(EntityWorldMut, mig)` değil: `spawn_player` ile simetrik —
   oyun spawn eder, kit göçle gelen kimliği damgalar (`Minter::arrival`)
   ve `Marker`'ı debug'da doğrular.
6. **`on_player_spawned` eklenmedi; takım ataması `TeamGame::team_of`.**
   Genel bir kanca her odada çağrılırdı ve demo, takımla ilgisi olmayan
   odalarda da `TeamMember` yazardı. Oda cevabı `TeamMember` olarak
   world'e kendisi yazar (sonraki değişimler düz bileşen yazımı).
7. **`KitMig<M>` `Deref<Target = M>`.** Oyunun alanları doğrudan
   okunur (`mig.pos`); kit'in yarısı (`park`; K1–K3'ten beri `input`
   de) kendi alanında.
8. **Demo'nun `Wire`'ı `(i32, i32)` değil `StripPos`.** `Strip = Wire`
   ve public `gsb_game::sharded::StripPos` yolu aynı tipi adlandırmalı;
   ayrıca `Planar`'ın oyuna ait bir wire tipi üzerinde çalıştığının ilk
   örneği.
9. **Anahtar genişlikleri:** `Team(pub u8)` (256 takım),
   `ConvexSectors2`'nin `Sector(pub u8)`'i (255 sektör + sınırlama;
   yapımda denetleniyor). Daha genişini isteyen oyun kendi
   `SectorMap`'ini yazar — oda anahtar üzerinden generic.
10. **Motor/tablo değişiklikleri:** `CellBook::last_cell` artık
    `(wire, cell)` tutuyor (despawn edilmiş entity'nin kimliği ancak
    buradan öğrenilir), `pending_removals` `(Entity, üye mi)`,
    yeni `sweep_removed::<Marker>`; `ShardedRoom`'un `own_wires` kümesi
    kalktı (`wire_entity`'nin anahtar kümesiyle birebir aynıydı), yerine
    ters harita `entity_wire` ve yönlendirme tablosu `route` geldi.
    Hiçbir alanın görünürlüğü genişletilmedi.

**Faz 5 eklemeleri** (kit düzeltme turu, §10 "Faz 5 sonucu"; hepsi
ekleme — bir tanesi, A3, oyun yazarı için kırıcı):

```rust
pub trait Game: Send + 'static {
    type Codec: RecordCodec;
    const SNAPSHOT_OP: u16;                         // A3: varsayılan YOK (eskiden 1003)
    const PRIVATE_OP: u16;                          // A3: varsayılan YOK (eskiden 1004)
    // … §4.5'teki kancalar aynı …
    fn may_release(&mut self, _w: &mut World, _entity: Entity) -> bool { true } // F4
}
pub trait TeamGame: Game {
    fn spawn_team_player(&mut self, w: &mut World, conn: ConnectionId)
        -> (Entity, Team) { /* spawn_player, sonra team_of */ }            // A1
    fn team_of(&mut self, world: &World, conn: ConnectionId, entity: Entity) -> Team;
}
pub trait Partition<W>: Send + 'static {
    // … §4.6'daki metotlar aynı …
    fn debug_check_wire(&self, _pos: &Self::Pos, _wire: &W) {}               // F3
}
impl<P> GridPartition2<P> { pub fn with_diagonals(self) -> Self }          // F2
// Her oda (OpenRoom, AoiRoom, TeamRoom, SectorRoom, ShardedRoom,
// ShardedSpatialRoom):
pub fn with_disconnect_policy(self, grace: Option<Duration>, to: ExpireTo) -> Self; // F4
```

- **`with_disconnect_policy(grace, to)`** (F4): `grace` —
  `Some(0)` park yok (anında despawn), `Some(d)` `d` bekletme, sonra
  oyunun `Game::may_release` vetosu durdukça (çıkış sayacı + savaşta
  çıkış yok; varsayılan kanca `true` → tam `d`'de biter), `None` veto
  kalkana dek bekletme (savaş kilidi; varsayılan kanca → bir sonraki
  tick); duran veto en çok çekirdeğin `RoomConfig::max_detach_hold`'u
  kadar (varsayılan 10 dk, kopuştan itibaren; RECONNECT §17); `to` — biten
  bekletme `ExpireTo::AiHandover` (varsayılan) ya da
  `ExpireTo::Despawn` (slot bırakılır). `with_disconnect_grace(g)`
  anlamını korur (`grace = Some(g)`, seçili `to` kalır); varsayılan
  politika değişmedi (30 sn → AI devri). Her oda çekirdeğin
  `GameLogic::may_release`'ini bekletilen oyuncunun entity'siyle
  oyuna iletir. Faz 5'te çekirdek vetoyu yalnız süresiz bekletmede
  soruyordu ("süreli bekletme + savaş vetosu" ifade edilemiyordu — §10
  "Faz 5 sonucu" gözlem); çekirdek artık vetoyu süreli bekletmenin
  deadline'ında da soruyor ve duran vetoyu `max_detach_hold` ile
  sınırlıyor (RECONNECT §17). Kit'te kod değişmedi: iletim zaten her
  odadaydı; MMO demosu ikisini birlikte kullanıyor (çıkış sayacı +
  savaşta çıkış yok).
- **`spawn_team_player`** (A1): takım odası her katılımda bunu çağırır
  (kit-içi `common::join_with` spawn adımını parametre alır).
  Varsayılanı eski iki adım; tek fark `team_of` artık kit wire
  kimliğini damgalamadan ÖNCE soruluyor (hiçbir kit oyunu orada kimliği
  okumuyordu). Ezen oyuna `team_of` sorulmaz (yine uygulanır — ör.
  `TeamMember`'ı geri okur).
- **`debug_check_wire`** (F3): sharded odalar şeridi yeniden kurarken
  ihraç ettikleri her entity için çağırır; `GridPartition2`'nin
  gövdesi `cfg!(debug_assertions)` içinde (release'de boş): wire
  izdüşümü konumunkinden bir şerit genişliğinden fazla uzaksa panik.
- **`with_diagonals`** (F2): 8-komşuluk; kenar komşuları listede önce
  (en kısa yollar eşitse rota kenarı seçer). Varsayılan 4-komşuluk.

**C1 eklemeleri** (seam ötesi okuma + uzak-etki, CROSS-SHARD §4b; hepsi
varsayılanlı — hiçbir mevcut oyun değişmedi, sharded olmayan odalar
hiçbirini görmez):

```rust
pub trait ShardGame: Game {
    // … Mig / capture / restore aynı …
    fn ingest_seam(&mut self, w: &mut World, ctx: &TickCtx, actions: &mut Vec<Action>,
                   players: &HashMap<PlayerId, Entity>, seq: &mut InputSeq,
                   seam: &mut Seam<'_, '_, Wire<Self>>) { /* ingest */ }
    fn systems_seam(&mut self, w: &mut World, ctx: &TickCtx,
                    seam: &mut Seam<'_, '_, Wire<Self>>) { /* systems */ }
    fn apply_remote_effect(&mut self, w: &mut World, target: Entity,
                           effect: &RemoteEffect, tick: u64,
                           seam: &mut Seam<'_, '_, Wire<Self>>) -> EffectOutcome
    { EffectOutcome::Rejected }
}
pub struct Seam<'s, 'a, V> { /* core CrossSeam + odanın wire→Entity tablosu */ }
impl<V> Seam<'_, '_, V> {
    pub fn local(&self, wire: u64) -> Option<Entity>;          // bu shard'ın entity'si
    pub fn lent(&self, wire: u64) -> Option<Lent<'_, V>>;      // komşunun ödünç kaydı (yerel değilse)
    pub fn lent_iter(&self) -> impl Iterator<Item = Lent<'_, V>>;
    pub fn tick(&self) -> u64;
    pub fn emit(&mut self, target: u64, source: u64, payload: Bytes)
        -> Result<EffectId, EmitRefused>;                      // yerel hedefe `Local`
}
```

- **Neden `ShardGame`'de ve varsayılanlı:** yalnız sharded odalar çağırır
  (`ShardGame`'in kendi gerekçesi, §4.6 sapma 4); varsayılanlar düz
  kancalara düşer, yani demo/arena/fikstür aynı kaldı. Elenen:
  `TickCtx`'e alan eklemek (tüm odaların ve bayt-sabitleme testinin —
  `kit_wire.rs` — struct literal'lerini kırar, sharded olmayan odaya
  anlamsız bir alan taşır); `World` kaynağı olarak koymak (ödünç görünüm
  aktörün alanı, kopyalamadan kaynağa konamaz).
- **Sahip kazanır:** `lent`/`lent_iter` odanın sahip-olunan wire'larını
  atlar (snapshot'taki own-wins filtresinin aynısı); `emit` yerel hedefi
  reddeder — yerel entity doğrudan yazılır.
- **`apply_remote_effect`:** kit `effect.target`'ı entity'ye çözer
  (canlı entity yoksa çekirdeğe `NoTarget`) ve kancayı değişim-penceresi
  koruması içinde çağırır; efektin yazımları/despawn'ları `update`'ten
  önce düşer, kirli geçiş ve silinen-tampon süpürmesi onları normal oyun
  yazımı gibi alır.
- Çekirdek tarafı (`gsb_core::shard`): `ShardLogic::{ingest_seam,
  update_seam, apply_remote_effect}` (varsayılanlı), `CrossSeam`,
  `Lent`, `RemoteEffect`, `EffectId`, `EffectOutcome`, `EmitRefused`,
  test/araç için `SeamStage`.

**C2 eklemeleri** (crystallization, CROSS-SHARD §4c; `ShardGame` BÜYÜMEDİ
— politika oda builder'ı, hepsi varsayılanlı ya da eklemeli; tek kırıcı
değişiklik `KitMig`'e alan eklenmesi, onu da yalnız kit kuruyor):

```rust
pub struct Crystallize { pub after: u64, pub window: u64,     // K, seri boşluğu
                         pub release: u64, pub margin: f32 }  // Default: 30/30/90/∞
impl ShardedRoom<G, P> { pub fn with_crystallize(self, policy: Crystallize) -> Self }
impl ShardedSpatialRoom<G, P, S> { pub fn with_crystallize(self, policy: Crystallize) -> Self }
pub trait Partition<W> {
    // … §4.6'daki metotlar aynı …
    fn holds(&self, _idx: usize, _pos: &Self::Pos, _margin: f32) -> bool { true } // bant
}
pub struct KitMig<M> { /* game, park, input */ pub pin: Option<ShardPin> }
pub struct ShardPin { pub partner: u64, pub last: u64 }
impl<V> Seam<'_, '_, V> {
    pub fn contact(&mut self, source: u64, target: u64);  // yerel darbeyi bildir
    // emit: gönderilen etki ya da `Local` reddi kontak sayılır
}
```

- **Karar kuralı:** `collect_migrations` bir entity'nin hedefini önce
  pin'inin `anchor`'ından, yoksa `region_of`'tan alır; tutulan çift
  bölgeden bağımsız kalır (CROSS-SHARD §4c madde 4). Pin mover'la
  `KitMig.pin` içinde gider; alıcı mover'ı ve partnerini pinler.
- **Kontak kaynakları:** `Seam::emit` (giden), odanın
  `apply_remote_effect`'i `Applied` dönünce (gelen), `Seam::contact`
  (oyunun yerel darbesi). Açılmamış odada üçü de no-op.
- **`GridPartition2::holds`:** bölge ya da dışarıda `margin`'den az
  (kötü eksende), `margin` border margin'ine kırpılır.
- Oda politikalarının builder'ları (`with_crystallize`,
  `with_disconnect_grace`, `with_disconnect_policy`) `room/policy.rs`
  çocuk modülüne taşındı (imzalar aynı).

**D eklemeleri** (göç tick'i, CROSS-SHARD §4d; kit'in public yüzeyi
DEĞİŞMEDİ — `ShardGame` ve `Seam` imzaları aynı, anlamları göç tick'inde
genişledi; çekirdeğe iki ekleme):

```rust
impl<S> CrossSeam<'_, S> {
    pub fn departed(&self, wire: u64) -> Option<usize>; // bu shard'ın az önce devrettiği entity'nin yeni sahibi
    // emit: kimse ödünç vermiyorsa `departed` sahibine (önce ödünç veren, eskisi gibi)
}
impl<S> SeamStage<S> { pub fn depart(&mut self, wire: u64, to: usize); } // commit'li göç (test aracı)
```

- **Göç tick'i:** `h`'de devredilen entity'nin `h + 1`'de eski dünyada
  duran kopyası, o tick'in ilk seam kancasından oyunun sistemleri bitene
  dek `Disabled`'dır (oyunun sorguları görmez) ve `Seam`'de yeni sahibin
  ödünç kaydıdır: `local` = `None`, `lent`/`lent_iter` `h`'de yakalanan
  kaydı yeni sahip kiralayan olarak verir, `emit` oraya gider; bot onu
  sürmez. Kit'in kendi geçişleri (orphan, crystal, border) onu eskisi
  gibi görür. Durum: `sharded/departing.rs` (`Departures`, bir tick'in
  göçleriyle sınırlı).
- **Sözleşme:** hedef wire'dan çözülür (dünya sorgusu ya da
  `Seam::local`) — saklanmış bir `Entity` tutamağıyla doğrudan yazım
  `Disabled`'ı atlar. `ShardGame::capture` belgesi bunu söyler.
- Elenen: *yeni `ShardGame` kancası ya da public `Seam::departing(wire)`*
  — oyunun yerel yolu dünya sorgusudur; her oyun her sorgusunu süzmek
  zorunda kalırdı (CROSS-SHARD §4d elenen 1); gizleme bunu kancasız yapar.

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

### 5.1 Faz 2: derlenen kit proto'su

`crates/gsb-kit/proto/kit.proto` (`package gsb.kit`, `import
"base.proto"`); Rust tarafı `gsb_kit::proto`:

```proto
message InputAck { uint64 processed_up_to = 1; }
message WorldSnapshot {
  uint64 sequence = 1;
  repeated bytes entities = 2;    // RecordCodec::encode gövdesi
  repeated uint64 removed = 3;    // kit PACKED olmayan biçimde yazar (bkz. aşağı)
  repeated bytes cell_exits = 4;  // CellSpace::encode_cell gövdesi
  bool delta = 5;
}
message Private {
  oneof payload { InputAck ack = 1; WorldSnapshot snapshot = 2; }
  repeated gsb.base.RpcResponse responses = 3;
  bytes game = 4;                 // oyunun kendi özel yükü (opak)
}
```

- **`Private.game = 4`:** 4 numarası `Private`'ta (ve bölündüğü
  `gsb.game.Private`'ta) hiç kullanılmadı (`git log -p` ile tarandı).
  Oneof'un dışında, `responses` gibi: bir tick hem ack hem oyun yükü
  taşıyabilir. Yuvayı dolduran kanca küçük paketteki G3-3 ile geldi
  (ilk kullanıcısı arena — takımı); aşağıda "Oturum yükü".
- **Oturum yükü — `Game::session_private(world, entity, out) -> bool`**
  (varsayılan `false`: hiçbir şey yazılmaz, kare ve karenin olup
  olmaması kancasızla birebir aynı — altı odada test kilitli). Kit onu
  **oturum başına bir kez** sorar: join'den ya da resume'dan sonraki
  İLK private karede (`InputSeq`'in `greet` bayrağı: `begin` — join ve
  artık resume — kurar, ilk kare tüketir; göç `adopt`'u kurmaz, oturum
  sürüyor). Yük, odanın o tick zaten gönderdiği karenin sonuna eklenir:
  AOI odalarında one-shot full'un, diğerlerinde ack/yanıt karesinin;
  başka bir şey yoksa kareyi tek başına kurar. Alan 4 karenin son alanı
  olduğu için elle eklemek üretilmiş kodlayıcının yazacağı baytın
  aynısıdır (etiket `0x22`, `common/session.rs`). `true` + boş gövde de
  gönderilir (`22 00`): sıfır değerli bir proto3 mesajı (arenada takım
  0) boş kodlanır, "yük yok" ile karışmamalı.
  - *Neden oturum başı, her tick değil:* ilk kullanıcının yükü (takım)
    oturum boyunca sabit; her tick sormak her bağlantıya her tick bir
    çağrı ve oyunda "zaten söyledim mi" defteri demekti. Resume'da
    yeniden söylenir: dönen istemci durumunu kaybetmiş yeni bir süreç
    olabilir. Sonradan DEĞİŞEN bir oturum yükü (takım değiştirme) ayrı
    bir tetikleyicidir — o gün kanca kit'e "yeniden söyle" diyen bir
    işaretle genişler; bugün RPC yanıtı ya da kayıt alanı yeter.
  - *Elenen:* ayrı bir "join" mesajı/opcode'u (oyunun opcode bloğuna
    yeni bir çerçeve, istemcide ikinci bir akış — `Private` zaten
    bağlantı başı yuva); yükü JOIN_ROOM_RESULT'a koymak (çekirdek zarfı,
    oyundan habersiz; resume'da da yok); yalnız one-shot full'a eklemek
    (takım odası ve düz odalar one-shot full göndermez — arena
    tetikleyicinin kendisi takım odasında). *T turu notu:* takım odası
    delta modunda artık one-shot full gönderiyor; oturum yükü, AOI'deki
    gibi, o karenin sonuna biniyor (arenanın takımı oynayan bir takıma
    katılan oyuncusu `Welcome`'ını one-shot full'un ardında alıyor).
  - İstemci yarısı: `ClientDecoder::session_private(&mut self, body)`
    (varsayılan: yok say) — `ClientView::apply_private` alan 4'ü
    bulursa zarf doğrulandıktan sonra, payload kolu uygulanmadan ÖNCE
    çözücüye verir; çözücünün hatası kareyi reddeder (görünüm
    değişmez, `errors` artar). Payload kolu olmayan, yalnız oturum yükü
    taşıyan kare `PrivateEvent::Session`'dır (`Empty` = yalnız RPC
    yanıtı): loadgen `Empty`'yi hata sayar, çünkü RPC göndermez — yeni
    varyant olmasa arenanın join karesi her istemcide bir hata olurdu.
- **`removed` packed değil:** kit her kimliği ayrı bir `0x18` etiketiyle
  yazıyor, üretilmiş bir kodlayıcı proto3'ün varsayılanı olan packed
  biçimi yazar; her protobuf ayrıştırıcısı ikisini de kabul eder. Bu
  yüzden proto'ya `[packed = false]` konmadı: demo'nun (değişmeyen)
  tipli aynası packed-varsayılan, iki tanımın üretilmiş kodlayıcıları
  aynı baytı yazmalı.
- **Oyunun tipli aynası:** demo'nun `game.proto`'su `import "kit.proto"`
  ile `InputAck`'i olduğu gibi kullanıyor (`gsb_demo::game::InputAck`
  yeniden ihraç), `WorldSnapshot` ve `Private`'ı tipli ayna olarak
  koruyor (aynı numaralar, `EntityRecord` / `CellExit` gövdeleri).
  Ayna `game = 4`'ü bildirmiyor (demo göndermiyor; ayrıştırıcı
  bilinmeyen alanı atlar) — bu sayede `wire_contract.rs`'in `Private`
  literal'i değişmedi. Özel yükü olan bir oyun o numarayı kendi mesaj
  tipiyle yazar.
- **Build:** `gsb-kit/build.rs` diğerlerinin kalıbında (gsb-lint,
  vendored protoc, `.gsb.base` için `extern_path`) ve proto dizinini
  `links = "gsb-kit-proto"` ile yayınlıyor (`DEP_GSB_KIT_PROTO_DIR`);
  `gsb-demo/build.rs` onu include edip `.gsb.kit` için `extern_path`
  veriyor (kit tipleri ikinci kez üretilmiyor).
- **Bayt uyumluluğu kilitli:** kit tarafında alan numaraları
  (`proto::tests`, bayt bayt), demo tarafında iki tanım (`tests/
  kit_wire.rs`): kurulmuş kareler (full; `removed` + `cell_exits` +
  kayıtlı delta; ack + yanıtlı ve one-shot full + yanıtlı `Private`)
  iki tanımda aynı baytı kodluyor ve birbirini kendine çözüyor; gerçek
  bir demo `AoiRoom`'unun elle yazdığı kareler (full, üç değişim
  türünü birden taşıyan delta, yanıtlı one-shot private full) iki
  tanımdan da aynı içeriğe çözülüyor ve aynı baytlara yeniden
  kodlanıyor. Mutation-check: `cell_exits = 4 → 6` üç testi de kırdı.

### 5.2 G4: kit'in referans istemcisi (`gsb_kit::client`)

Zarfın istemci yarısı da artık kit'te (GAME-MODULE §5 "G4 sonucu";
önce altı kopyaydı: loadgen, `delta_aoi.rs`, `aoi.rs`, MMO'nun iki test
istemcisi, örnek istemci). `ClientView<D: ClientDecoder>` bir
bağlantının `WorldSnapshot` ve `Private` karelerini **ham bayt olarak**
alır ve `kit.proto`'daki istemci kurallarını tam olarak uygular: full
görünümü değiştirir; baseline'lı delta `removed` → `cell_exits` →
upsert sırasıyla, boşluk olsa da uygulanır; baseline'sız delta düşer;
son KABUL EDİLMİŞ sequence'tan `<=` olan atılır (ilk kabulden önce
hiçbir şey bayat değildir); one-shot private full koşulsuz uygulanır ve
sequence'ını benimser; private delta hatadır. Sayaçlar (`fulls` —
private dahil —, `private_fulls`, `deltas`, `gap_drops`, `stale`,
`errors`) loadgen'in raporladıklarıdır.

Oyunun seam'i `ClientDecoder`: kayıt gövdesi → `(wire id, Record)`,
`cell_of(&Record)` (sunucunun `CellSpace` formülü, yalnız çıkış
servis edilirken çağrılır), çıkış gövdesi → hücre. Gövdeler `RecordCodec
::encode` / `CellSpace::encode_cell` çıktısıdır; çözücü onları tipli
aynayla (`decode(body)?` — `From<prost::DecodeError>`) ya da public
yürüyücüyle (`client::wire::{Fields, Value, sint32}`) çözer. Kare iki
yürüyüşte uygulanır, ayırma yok: birincisi başlığı okur, (az sayıdaki)
`removed` ve `cell_exits`'i yeniden kullanılan scratch'e çözer ve tüm
zarfı doğrular; ikincisi yalnız kayıt aralığını yürüyüp her kaydı
doğrudan görünüme yazar (görünüm başına kayıt ara belleği yok —
binlerce görünümde önbellekte ıskalanan bellek olurdu). Bozuk zarf
görünümü değiştirmez; oyunun reddettiği bir kayıt gövdesi görünümü boş
ve baseline'sız bırakır (asla yarım kare). `Private.responses`
görünümün parçası değil; `Private.game` (oturum yükü, §5.1) de değil —
çözücünün `session_private` kancasına gider. `tokio` bağımlılığı yok (saf
durum).

## 6. Hareket bir trait değildir

Hareket oynanışın kendisidir: FPS'te fizik, zıplama, eğilme; MOBA'da
tıkla-git ve yol bulma; izometrik oyunda grid. Hepsini kapsayan bir trait
ya o kadar ince olur ki `gsb-ecs`'teki `System` trait'inden farkı kalmaz,
ya da tek bir oyun türüne göre kesilir. Oyunun sistemleri `Game::systems`
ile çalışır; kit yalnızca **isteğe bağlı** hazır yardımcılar sunar (§7).

## 7. Hazır ön-ayarlar

Oyun yazarının her şeyi sıfırdan uygulamaması için kit, trait'lerin
yaygın durumlar için hazır uygulamalarını taşır:

| Ön-ayar | İçerik | Durum |
|---|---|---|
| `Pos2<i32>`, `Pos2<f32>`, `Pos3<f32>` | konum bileşenleri + `RecordCodec` (farklı nicemleme seçenekleriyle) | kurulmadı (§10 "Faz 1a sonucu" 5) |
| `Grid2`, `Grid3` | `CellSpace` (2D'de 3×3, 3D'de 27 hücre görünüm) | `Grid2` 1a'da; `Grid3` **tetikleyici: hacimsel AOI isteyen bir oyun** (kontrol demolarından hiçbiri kullanmıyor — MMO yer düzleminde `Grid2`) |
| `VisionGrid2`, `VisionGrid3` | `Vision` (yarıçap boyutlu ızgara + kesin mesafe testi; 2D'de 3×3, 3D'de 27 hücre komşuluk) | `VisionGrid2` 1b'de; `VisionGrid3` Faz 2'de (arenanın takım sisi) |
| `ConvexSectors2` | 2D dışbükey çokgen sektörlerle `SectorMap` | 1b'de |
| `GridPartition2`, `GridPartition3` | ızgara `Partition` (bugünkü 2D bölme bunun ilk örneği); `GridPartition2` 4-komşuluk, `with_diagonals()` ile 8-komşuluk (köşeden şerit + tek adımlık köşegen göç — Faz 5, F2) | `GridPartition2` 1b'de (8-komşuluk Faz 5'te); `GridPartition3` **tetikleyici: 3D sharding isteyen bir oyun** (MMO yer düzleminde `GridPartition2`, 8-komşuluk) |
| `KinematicMover<P>` | isteğe bağlı "hedefe doğru ilerle" sistemi, 2D/3D | kurulmadı |
| `ClientView<D>` + `ClientDecoder` (`gsb_kit::client`) | istemci tarafı: kit zarfının referans uygulayıcısı (istemci kuralları, sayaçlar) + zarf yürüyücüsü `client::wire` (§5.2) | GAME-MODULE G4'te; kullanıcıları loadgen, demo/MMO test istemcileri, örnek istemci |

Ön-ayarlar oyunun tiplerini somut bir konum tipi üzerinden değil
**erişimci trait'ler** üzerinden okur: 2D ön-ayarlar `Planar`'ı
(`[Coord; 2]`; simülasyon `f32`, wire `i32`), 3D ön-ayarlar
`Spatial`'ı (`[Coord; 3]`; Faz 2'de kuruldu, bugün tek okuyucusu
`VisionGrid3`). Tasarım ve elenen alternatifler: §4.6. **Birim
sözleşmesi** (Faz 5, F3): hem konumu hem wire'ı okuyan ön-ayar
(`GridPartition2`) ikisini tek birimde karşılaştırır — oyunun konum
`Planar`'ı ile wire `Planar`'ı AYNI birimi raporlamalı (konumdan ince
nicemlenen wire, konumun birimine geri izdüşürülür); debug build'de
`Partition::debug_check_wire` denetler.

İzometrik bir oyun kendi 2D konumuna `Planar` uygular ve `Grid2` takar;
bir FPS `Pos3`'ünü seçer ve AOI'yi yer düzleminde (`Planar` → `[x, z]`
+ `Grid2`) ya da hacimsel (`Spatial` + `Grid3` — tetikleyiciyle kurulur)
yapabilir. Alışılmadık
bir oyun kendi tipine seam trait'lerini doğrudan uygular.

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
   **Faz 1b'de kanıtlandı ve kapandı** (`3090fa3`): grup hiç çıkış
   almıyordu, keep-alive full'u ölü NPC'yi taşımaya devam ediyordu;
   defter artık silinen-bileşen tamponlarını (`WireId` + `Marker`)
   kapanıştan önce okuyor, shard tabloları da.
3. **`clear_trackers` yalnız iki odada çağrılıyor** (AOI ve spatial
   kompozit). Open, Team, PVS ve `ShardedRoom`'da silinen-bileşen
   tamponlarının büyüdüğünden şüpheleniliyor. **Ölçülmedi.**
   **Faz 1a:** `OpenRoom` için testle kanıtlandı (her `on_leave`
   despawn'ı odanın ömrü boyunca tamponda kalıyordu) ve kapandı;
   **Faz 1b:** Team, PVS ve `ShardedRoom` için aynı test kırıldı ve
   kapandı (`9aa398b`).
4. **Sabit sınırlar:** takım sayısı 2'ye sabit (`[_; 2]` diziler), PVS
   görünürlük tablosu `u16` bitmask olduğu için 16 sektörle sınırlı,
   bitişik olmayan bir bölgeye düşen entity hiçbir komşuya göç
   ettirilmiyor. **Faz 1b'de üçü de kanıtlandı ve kapandı:** takımlar
   (`db434b6`), sektörler (`1873ca6`), bitişik olmayan göç — komşu
   üzerinden adım adım yönlendirme, çekirdek değişmeden (`5f6bd3d`).
5. **Varlık tipine bağlı göç:** `Speed` bileşeni olmayan bir NPC bugün
   hiç göç etmiyor (`sharded/room/shard.rs:50` sorgusu `&Speed` istiyor).
   **Faz 1b'de kanıtlandı ve kapandı** (`a9b37f2`): kit `Marker`'ı
   taşıyan her entity'yi göç ettiriyor, neyin taşındığı oyunun
   `capture`'ı.

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
  *(Sonradan yapıldı — `docs/GAME-MODULE.md` G1–G4: `GameModule`
  trait'i, sunucu ve loadgen her barındırılan oyunu sürüyor.)*

## 10. Fazlar

| Faz | Kapsam | Büyüklük | Kapı |
|---|---|---|---|
| 0 | `gsb-game` içinde modül bölmesi: `kit/` ve `demo/`, geçici bir ara modül; davranış değişmez | ~1 gün | tüm testler değişmeden yeşil — **tamam**, aşağıda "Faz 0 sonucu" |
| 1 | Bağımlılığın ters çevrilmesi: §4 trait'leri, generic `CellBook`/`CellPieces`, kit'e ait `WireId`/basım/Private zarfı, sharded park/join kopyalarının birleştirilmesi, §8 açıklarının testle doğrulanıp kapatılması, kit için küçük bir 2D test oyunu — iki tura bölündü: **1a tamam** (aşağıda "Faz 1a sonucu"), **1b tamam** (aşağıda "Faz 1b sonucu": takım sisi, PVS, sharded kompozitler, §8.2–§8.5, seam kit zarfına indi) | ~3,5 bin satır dokunulur, +400–600 yeni | **baytlar birebir aynı** + loadgen gürültü içinde |
| 2 | Crate bölmesi: `gsb-kit` (+ kendi proto'su) ve `gsb-demo`; `gsb-server` yolları; arenanın 3D ön-ayarı (`Spatial`, `VisionGrid3`) — **tamam**, aşağıda "Faz 2 sonucu" | ~400–600 satır, çoğu yol | tüm testler + loadgen |
| 3 | 3D arena demosu (`gsb-demo-arena`): bileşenler, hareket, codec, savaş sisi / takım görüşü (`Vision` + `VisionGrid3`), proto — **tamam, kit ve çekirdek dokunulmadan**, aşağıda "Faz 3 sonucu" (iki engelleyici olmayan tasarım bulgusu) | ~800–1 200 satır | kabul kriteri 1 (§11) |
| 4 | 3D MMO demosu (`gsb-demo-mmo`): büyük dünya, sharded × spatial, NPC'ler, park/bot (§13) — **tamam, kit ve çekirdek dokunulmadan**, aşağıda "Faz 4 sonucu" (dört tasarım bulgusu, biri doğruluk hatası) | ~1 000–1 500 satır | **kapanış doğrulaması** — üç demo birlikte |
| 5 | Kit düzeltme turu: Faz 3 + Faz 4 bulguları (F1–F4, A1–A2) ve opcode varsayılanları (A3), her biri kendi commit'inde, önce kırılan testiyle; demolar düzeltmeleri kullanıyor, bulgu kilitleri çevrildi — **tamam**, aşağıda "Faz 5 sonucu" | ~1 700 satır (çoğu test) | baytlar aynı, çekirdek dokunulmadan, kapanış kontrolü + loadgen gürültü içinde |

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
   bekliyor (1b ya da kullanıcı). *(1b: karar verildi — her kit odası
   yönlendiriyor; "Faz 1b sonucu".)*
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

### Faz 1b sonucu

**Tamamlandı** (`kit/phase-1b`, `81c0186..`; CHANGELOG "gsb-kit Faz 1b
turu"). **Faz 1 bitti:** her strateji odası oyun üzerinden generic,
kit demo'ya yalnız kendi zarf tipleri için ulaşıyor.

- **Odalar:** `TeamRoom<G: TeamGame, V: Vision>`, `SectorRoom<G: Game,
  M: SectorMap>`, `ShardedRoom<G: ShardGame, P: Partition<Wire<G>>>`
  (`State = KitMig<G::Mig>`, `Strip = Wire<G>`),
  `ShardedSpatialRoom<G, P, S: CellSpace<Wire<G>>>`. İmzalar, konum
  erişimcisi (`Planar`) ve her sapma: §4.6.
- **Ön-ayarlar:** `Grid2` (artık `Planar` üzerinden), `VisionGrid2<P>`,
  `ConvexSectors2<P>` (`in_convex` kit'e döndü), `GridPartition2<P>`;
  hepsi oyunun tiplerini `Planar` ile okuyor (§7).
- **Ortak makine:** sharded park kopyası ortak `park.rs` ile birleşti
  (§4.4 — aynı `ParkEntry` defteri ve kanca gövdeleri; göçte kayıt
  `ShardParkRecord` olarak `KitMig` içinde taşınıyor). Join / girdi /
  sistemler / istek yolları ve snapshot zarfları her odada kit'in;
  değişiklik penceresini her oda tick başına bir kez kapatıyor.
- **İstek yönlendirmesi tekdüze** (ebeveyn kararı, 1a'nın açık
  noktası 2): bütün kit odaları `Game::handle_request`'e soruyor —
  AOI / takım / PVS için bilinçli davranış değişikliği (CHANGELOG).
- **Demo:** `DemoGame` artık `TeamGame` (conn paritesi) ve
  `ShardGame` (`DemoMig`: konum, varsa hız, hedef) da uyguluyor;
  `Position` ve `StripPos` `Planar` uyguluyor; harita yalnız veri
  (`demo_map`). Eski yollar tip takma adı, kurucular `demo/rooms.rs`'te;
  `crates/gsb-game` dışında tek dosya değişmedi.

**Son seam içeriği** (`kit/seam.rs`) — test dışı yalnız kit zarfı:

```rust
pub(crate) use crate::demo::game::{InputAck, Private, private};  // Faz 2: kit proto'su
#[cfg(test)] pub(crate) use crate::demo::game::WorldSnapshot;     // testler çözüyor
#[cfg(test)] pub(crate) use crate::demo::game::CellExit;
#[cfg(test)] pub(crate) use crate::demo::play::DemoGame;
#[cfg(test)] pub(crate) use crate::demo::components::{DEFAULT_SPEED, MoveTarget, Position, Speed};
#[cfg(test)] pub(crate) use crate::demo::migrate::DemoMig;
#[cfg(test)] pub(crate) use crate::demo::wire::StripPos;
#[cfg(test)] pub(crate) use crate::demo::sectors::{SECTOR_EAST, SECTOR_NW, SECTOR_OUT, SECTOR_WEST};
```

Game kancası, kodek, sektör ya da bölme öğesi kalmadı (1a sonunda 25
öğe / 19 `use` satırı + 6 test öğesiydi; şimdi 3 zarf öğesi / 1 satır +
14 test fikstürü). Seam, Faz 2'de zarf kit proto'suna geçince silinir.

**§8 açıklarının çözümü** (her biri önce kırılan testiyle, ayrı
commit'te):

| Açık | Kırılan test (düzeltmeden önce) | Düzeltme | Commit |
|---|---|---|---|
| §8.2 hayalet | AOI: oyunun despawn ettiği NPC için grup paketi hiç çıkmıyor ("the despawn is news for the group"); keep-alive full'u ölüyü taşıyor (`[1, 2]` ≠ `[1]`); spatial kompozit aynı | `CellBook::last_cell` `(wire, cell)`; `sweep_removed::<Marker>` silinen-bileşen tamponlarını (`WireId` + `Marker`) park edilen çıkışlardan sonra, pencere kapanmadan okur; shard tabloları `entity_wire` ile unutur; göçle çıkan NPC artık üye sayılmıyor | `3090fa3` |
| §8.3 pencere | takım / PVS / düz sharded: "tick 1: the update closed the window: left 1, right 0" | her `update` sonunda `close_change_window` | `9aa398b` |
| §8.4 takımlar | 3 takım: "index out of bounds: the len is 2 but the index is 2" | takım numarasıyla indekslenen, büyüyen tablolar; ızgara `(hücre, takım)` | `db434b6` |
| §8.4 sektörler | 20 sektör: "attempt to shift left with overflow" (release'te sessiz yanlış görünürlük) | bit maskesi yerine sektör başına liste; anahtar `u8` → 255 sektör | `1873ca6` |
| §8.4 bitişik olmayan göç | 2×2 köşegen: "handed to exactly one neighbour (to 1: 0, to 2: 0)" | komşu grafı üzerinde BFS yönlendirme tablosu; ara shard kurup sonraki tick'te iletir | `5f6bd3d` |
| §8.5 `Speed`'siz göç | "the NPC's crossing is reported to shard 1: left 0, right 1" | göç sorgusu `Marker` + bölme konumu; taşınan durum oyunun `capture`'ı | `a9b37f2` |

**Tasarımla çelişen / tasarımın öngörmediği (kayıt):**

1. **§4.3'ün `Mig` / `capture` / `restore`'u `Game`'de değil**
   (`ShardGame`, §4.6 sapma 4) ve **`on_player_spawned` yok**
   (`TeamGame::team_of`, sapma 6).
2. **Bitişik olmayan göç çekirdek değişmeden ancak adım adım
   çözülebiliyor.** Çekirdek `collect_migrations`'ı yalnız
   `neighbors()` için çağırıyor ve shard'lar yalnız komşularına gerçek
   link tutuyor; sahibe doğrudan teslim, `neighbors()`'ı bütün shard'lar
   yapmak (sınır değişimi herkes-herkese) demekti. Bedeli: adım başına
   bir tick ve ara shard'ın entity'yi o tick boyunca kendi dünyasında
   taşıması (snapshot'ında ve sınır ihracında görünür). Sözleşme: bir
   `Partition`'ın komşuluk grafı bağlantılı olmalı (değilse ulaşılamayan
   bölgeye giden entity bekler — `first_hops` onu hiç raporlamaz).
3. **§4.4'ün "tek `clear_trackers` sahibi" kuralı artık doğruluk
   taşıyor:** hayalet süpürmesi, iki kapanış arasındaki her despawn'ın
   tamponda tam bir kez bulunmasına dayanıyor (bir kanca pencereyi
   kapatırsa despawn'lar kaybolur — debug bekçisi bunu yakalıyor).
4. **Göçle çıkan NPC'nin üyelik muhasebesi yanlıştı** (her park edilen
   çıkış üye sayılıyordu); §8.5 düzeltmesiyle NPC'ler göç etmeye
   başlayınca canlı bir hata olurdu. §8.2 commit'inde testle kapandı.
5. **Faz 2 tuzağı büyüdü:** demo'nun kurucuları artık altı generic kit
   tipi üzerinde inherent impl (`OpenRoom`, `AoiRoom`, `TeamRoom`,
   `SectorRoom`, `ShardedRoom`, `ShardedSpatialRoom`); crate bölmesinde
   serbest fonksiyona ya da uzantı trait'ine dönmeli.
6. **`kit::aoi::Cell` yeniden ihracı kalktı** (tek tüketicisi takım
   odasıydı); `gsb_game::aoi::Cell` kökten aynı. AOI'nin sabit test
   dosyaları hücreyi `#[cfg(test)]` bir import'la alıyor.
7. **Sabit olmayan bir çekirdek testi bir kez kırıldı:**
   `room::tests::fanout::keepalive::never_emitted_group_warns_once_naming_the_group`
   (20 ms uykulu zamanlama testi, `gsb-core` dokunulmadı); yeniden
   koşularda geçti.

**Görünürlük:** hiçbir alan genişletilmedi. Yeni public öğeler:
`Planar`, `Vision`, `VisionGrid2`, `SectorMap`, `ConvexSectors2`,
`Sector`, `Partition`, `GridPartition2` (`kit::space`; `grid_shape`,
`shard_at` oraya taşındı), `TeamGame`, `ShardGame`, `KitMig`, odaların
`with_game` / `with_shard` / `game` / `game_mut`'ı, `DemoMig`,
`demo_map`, `gsb_game::sharded::{KitMig, DemoMig}`. Kaldırılan:
`ShardedRoomState` yapısı (kök yolu artık `KitMig<DemoMig>` takma adı),
`kit::team::TEAM_COUNT` (demo'ya; kök yolu aynı). Alan değişiklikleri:
§4.6 sapma 10.

**Sabit testlere dokunuş:** `git diff 81c0186.. --stat` —
`crates/gsb-game/tests/` boş (`wire_contract.rs`, `delta_aoi.rs`,
`team.rs`, `pvs.rs`, `park_policy.rs` … hiç değişmedi),
`aoi/tests/sharing*.rs` boş; modül içi test dosyalarına yalnız tip
takma adları (`TeamRoom`, `SectorRoom`, `ShardedRoom`,
`ShardedSpatialRoom` → demo örneklemesi), yeni çocuk test modüllerinin
`mod` satırları ve elle kurulan iki göç durumunun yeni şekli
(`ShardedRoomState { pos, speed, target, park }` → `KitMig { game:
DemoMig { … }, park }`, hız `Option`) eklendi. Beklenen bayt, kayıt
sayısı ya da assertion değişmedi. Ek olarak, yalnız public API'yi
kullanan geçici bir eşdeğerlik koşumu (commit'lenmedi) altı stratejiyi
900 tick boyunca rastgele hareket, ayrılış ve yeniden katılımla
sürdü; her snapshot / keep-alive / private çıktısının kanonik özeti
taban ve HEAD'de birebir aynı çıktı.

**Doğrulama:** 433 test (419 + 14) / 0 hata / 1 ignored. Loadgen (50
istemci), 81c0186 ↔ HEAD dönüşümlü (ölçülen ikili `6ac1e3d`'nin;
sonraki commit'ler yalnız yorum, biçim ve doküman):

| Koşu | left | errors | server_closes | step_p50_fine_us (taban / HEAD, 3 çift) | snap_total (taban / HEAD) | out_bps_per_conn (taban / HEAD) |
|---|---|---|---|---|---|---|
| tcp 3 sn | 50 / 50 | 0 / 0 | 0 / 0 | 64 64 48 / 64 80 48 | 4100 / 4100 | 11044 / 11023 |
| udp 3 sn | 50 / 50 | 0 / 0 | 0 / 0 | 56 64 40 / 56 64 32 | 4100 / 4100 | 11043 / 10987 |
| spatial 3 sn | 50 / 50 | 0 / 0 | 0 / 0 | 104 128 104 / 120 136 128 | 4091 / 4092 | 2012 / 2014 |
| team 3 sn | 50 / 50 | 0 / 0 | 0 / 0 | 72 80 72 / 64 80 56 | 4100 / 4100 | 11013 / 11003 |
| pvs 3 sn | 50 / 50 | 0 / 0 | 0 / 0 | 72 64 64 / 72 88 48 | 4150 / 4150 | 6118 / 5964 |
| sharded N=4, 8 sn | 50 / 50 | 0 / 0 | 0 / 0 | 40 40 32 / 40 40 32 | 11516 / 11466 | 7077 / 7005 |
| sharded × spatial N=4, 8 sn | 50 / 50 | 0 / 0 | 0 / 0 | 64 64 72 / 64 64 72 | 11475 / 11479 | 3318 / 3303 |

snap_total / out_bps ilk çiftin değerleri. `spatial`'in üç çiftinde
HEAD 1–3 kova yukarıdaydı; altı çift daha alındı: taban 136 136 120 120
120 128, HEAD 136 120 120 120 136 152 — dokuz çiftte ortalama fark bir
kova (8 µs), işaret çiftten çifte değişiyor; 10 sn'lik dört çift de
karışık (taban 120 80 80 104, HEAD 120 104 96 96). Aynı senaryoyu tek
çekirdekte süren zamanlama koşumu `ingest + update` için tick başına
taban 37,4 / 40,2 µs, HEAD 40,7 / 40,5 µs verdi. Mantık birebir aynı
(yukarıdaki eşdeğerlik koşumu); fark gürültü içinde sayıldı.

### Faz 2 sonucu

**Tamamlandı** (`kit/phase-2`, `c97270b..`; CHANGELOG "gsb-kit Faz 2
turu"). Workspace'te artık `crates/gsb-kit` ve `crates/gsb-demo`
(yeniden adlandırılan `gsb-game`, `git mv`) var; kit hiçbir profilde
demo'ya bağlı değil.

**Crate grafı** (`cargo tree --depth 1 -e normal,build,dev`, yalnız
`gsb-*`):

| Crate | Bağımlılıklar (`gsb-*`) |
|---|---|
| `gsb-lint` | — |
| `gsb-protocol` | build: `gsb-lint` |
| `gsb-ecs` | build: `gsb-lint` |
| `gsb-core` | `gsb-protocol`; build: `gsb-lint` |
| `gsb-net` | `gsb-core`, `gsb-protocol`; build: `gsb-lint` |
| **`gsb-kit`** | `gsb-core`, `gsb-protocol`; build: `gsb-lint` (ayrıca `bevy_ecs`, `prost`, `bytes`) |
| **`gsb-demo`** | `gsb-core`, `gsb-ecs`, **`gsb-kit`**, `gsb-protocol`; build: `gsb-lint` |
| `gsb-server` | `gsb-core`, `gsb-demo`, `gsb-ecs`, `gsb-net`, `gsb-protocol`; build: `gsb-lint` |

`cargo tree -p gsb-kit -e normal,dev,build | grep -c gsb-demo` → `0`.
Kit `gsb-ecs`'e de bağlı değil: tek kullanımı (`run_systems`, bir
`SystemRunner` çağrısı) yalnız demo'nundu ve demo'nun `Game::systems`'ine
taşındı.

**Katman kuralı artık yapısal.** Faz 0'ın kaynak-tarayan katman testi
(`src/layering.rs`, 2 test) emekli edildi: kit ayrı bir crate ve
`gsb-kit`'e normal bir `gsb-demo` bağımlılığı eklemek cargo'da derlenmez
("cyclic package dependency: package `gsb-demo` … depends on itself").
Dev-dependency döngüsünü cargo kabul ediyor (kit'in ikinci bir kopyasını
derler; test kopyasının tipleri oyununkiler olmaz) — o açığı kit'in
tek manifest testi kapatıyor (`the_kit_manifest_names_no_game_crate`:
bağımlılık tablolarından herhangi biri motor dışı bir `gsb-*` crate
adlandırırsa kırılır; `[dev-dependencies] gsb-demo` eklenerek
mutation-check edildi).

**Kit proto'su:** §5.1 (alan numaraları, `Private.game = 4`, `removed`'ın
packed olmayan yazımı, tipli ayna, build, bayt-uyumluluk kilidi).

**Demo kurucuları: uzantı trait'leri.** Demo'nun kurucuları kit'in altı
generic oda tipi üzerinde inherent impl'di — crate sınırında E0116. Her
oda için bir demo trait'i (`OpenRoomExt`, `AoiRoomExt`, `TeamRoomExt`,
`SectorRoomExt`, `ShardedRoomExt`, `ShardedSpatialRoomExt`); hepsi odayı
adlandıran uyumluluk modülünden ve `gsb_demo::prelude`'dan ihraç
ediliyor. Çağrı sözdizimi aynı (`OpenRoom::new()`,
`AoiRoom::with_spawn_half(c, h).with_disconnect_grace(g)`,
`.with_economy(e)`); `gsb-server`'ın fabrikalarına ve demo testlerine
tek satır `use gsb_demo::prelude::*;` eklendi. Elenen: serbest
fonksiyonlar (her çağrı yeri yeniden yazılır, `with_economy` builder
zincirini iç içe çağrıya çevirir), newtype sarmalayıcılar (her
`GameLogic` / `ShardLogic` metodu altı kez delege), kit'te oyun
üzerinden generic kurucular (kit demo'nun spawn haritasını, harita
verisini, ekonomi servisini bilemez). `OpenRoom<DemoGame>` ve
`SectorRoom<DemoGame, _>`'in `Default` impl'leri aynı yetim kuralıyla
kalktı — çağıranı yoktu.

**Fikstür oyunu** (`gsb-kit/src/testing/`, yalnız `#[cfg(test)]`,
public API'nin dışında — oyun yazarına örnek `gsb-demo`'nun kendisi):
`Fixture` — `Position` / `Speed` / `MoveTarget`, kesen bir kodek
(`FixCodec`, gövde `{ uint64 entity = 1; sint32 x = 2; sint32 y = 3; }`),
conn paritesiyle takım, konum + hız + hedef taşıyan göç (`FixMig`),
dört sektörlü harita, kit zarfının prost-derive tipli aynaları
(`WorldSnapshot`, `Private`), altı oda için kurucular. Girdi çözmez,
sistemi yok, oyuncuları orijine spawn eder (testler yerleştirir).
Mevcut sarmalayıcılar (`Culling`, `Recording`, `ThreeTeams`,
`ClearsTrackers`) artık onu sarıyor.

**Test göçü** (e80d2e4'te `gsb-game`'in 96 testi → bugün):

| Grup | Sayı | Yeni yer |
|---|---|---|
| Kit'in modül içi testleri (kodeğin yazdığı değere bakmayan: fikstür üzerinde ya da oyundan bağımsız — `put_delimited`, iki `Minter`, bölme/yönlendirme, 3D yer düzlemi kilidi) | 51 | `gsb-kit` modül içi, aynı ad ve aynı modül yolu (`kit::` öneki düştü) |
| `WireId` `compile_fail` doctest | 1 | `gsb-kit` (yol `gsb_kit::identity::WireId`; hâlâ doğru sebeple — E0603 özel kurucu — kırıldığı doğrulandı) |
| Demo kodeğinin yazdığı DEĞERE bakan kit testleri | 11 | `gsb-demo` `src/demo/rooms/tests/{open,aoi,pvs,sharded}.rs`, aynı ad |
| Kit bekçisi `a_hook_closing_the_change_window_is_caught` (demo'daydı) | 1 | `gsb-kit` `room::tests::change_window`, fikstürle |
| Demo birim testleri (kodek ×2, `SECTOR_OUT`) | 3 | `gsb-demo`, aynen |
| Entegrasyon testleri (`tests/*.rs`, 9 dosya) | 27 | `gsb-demo/tests/`, aynı dosyalar (yalnız `gsb_game` → `gsb_demo` ve bir `prelude` import'u) |
| Katman tarayıcısı (`layering`) | 2 | **emekli** (yukarıda) |

Demo'ya taşınan 11 test: `aoi_tick_cache_no_stale_block`,
`aoi_silent_delta_silent_no_stale`, `aoi_two_groups_same_cell_same_block`,
`aoi_structural_dirty_direct_write`, `aoi_partial_delta_only_mover_recorded`,
`aoi_quantized_move_no_wire_change_no_record`,
`sector_transition_snapshot_and_identity`,
`borrowed_border_entities_render_without_gap_across_seam`,
`delta_bookkeeping_ignores_unchanged_borrowed_strip`,
`snapshot_emits_on_plain_position_write`,
`entity_spawned_outside_on_join_is_broadcast_with_fresh_wire_id`. Ölçüt
test başına: bir assertion'ın beklenen değeri demo kodeğinin yazdığı
bir kayıttan (çözülmüş, kesilmiş koordinat) ya da nicemlemesinden
geliyorsa test demo'nundur. Beklenen bayt, kayıt sayısı ve assertion
hiçbirinde değişmedi; yalnız tesisat: oyuncunun entity'si odanın özel
tablosundan değil public wire id'sinden bulunuyor (`entity_of`), istemci
görünümü hücreyi kit'in `pub(crate)` `cell_of`'u yerine `Grid2`'nin
public `CellSpace::cell_of`'uyla hesaplıyor. Taban ↔ HEAD her testin
gövdesi normalize edilip karşılaştırıldı (tip adları, import yolları):
fark yalnız bu tesisat satırlarında ve aşağıdaki tek düzeltmede.

**Gizli bir test hatası bulundu ve düzeltildi.**
`aoi_join_leave_same_tick_inert`, `ConnectionId(9)` ile katılıp
`PlayerId(9)`'u bırakıyordu — oysa katılım `PlayerId(2)` basar: ayrılış
hiç olmuyordu, test yalnız demo conn 9'u gözlenen hücrenin dışına spawn
ettiği için geçiyordu (fikstür orijine spawn edince kırıldı: "the member
counts are intact", 2 ≠ 1). Test artık katılımın döndürdüğü oyuncuyu
bırakıyor; aynı düzeltme e80d2e4'ün kodunda da geçiyor (orada
`joined.player != PlayerId(9)` da doğrulandı). Assertion'lar aynı.

**Sayılar:** 433 − 2 (emekli katman testleri) + 8 yeni = **439** / 0 hata
/ 1 ignored. Yeni: kit zarfının alan numaraları ×2, `kit_wire` ×2,
manifest ×1, `VisionGrid3` ×2, üç takımlı 3D takım sisi ×1.

**3D ön-ayarlar — var olan ve tetikleyici bekleyen.**
- Kuruldu: `Spatial { type Coord; fn spatial(&self) -> [Coord; 3] }`
  (§4.6'nın öngördüğü imza), `Cell3`, `VisionGrid3<P>` (`Vision`:
  yarıçap kenarlı küp hücreler, 27 hücrelik komşuluk, kesin 3D mesafe,
  yarıçap `VisionGrid2` gibi sınırlanıyor). Testler: yükseklik ayırıyor
  (düzlem ön-ayarı aynı noktaya koyduğu iki birimi 3D ön-ayar
  ayırıyor), komşuluk 27 farklı hücre ve ızgara sözleşmesini sınır
  üstü bir kafeste tutuyor, üç takımlı bir `TeamRoom<_, VisionGrid3>`
  bir düşmanın tam üstündeki birimi o takımın paketinden dışarıda
  bırakıyor. Mutation-check: yükseklik terimi atılınca birinci ve oda
  testi, iki katmanlı komşulukta sözleşme testi kırıldı.
- MMO'nun yolu değişmedi ve kilitli: 3D `Pos3`/`Wire3` `Planar`'ı
  `[x, z]` olarak uygulayıp `Grid2` + `GridPartition2`'yi kullanıyor
  (`space::tests::a_ground_plane_3d_game_uses_the_planar_presets_unchanged`,
  fikstürden bağımsız, aynen taşındı; aynı `Pos3` artık `Spatial`'ı da
  uyguluyor — bir tip iki erişimciyi birden).
- **Kurulmadı (tetikleyici):** `Grid3` (hacimsel AOI) ve `GridPartition3`
  (3D sharding) — kontrol demolarının hiçbiri kullanmıyor (§7).

**Public yüzeydeki değişiklikler** (derleyici buldu, ikisi de
genişletme değil zorunluluk): `InputSeq` `Game::ingest`'in imzasında
ama özel `common` modülündeydi — `gsb_kit::game::InputSeq` yeniden
ihracı; `run_systems` demo'ya taşındı. Kit'te `pub(in crate::kit)` →
`pub(crate)`, `pub(in crate::kit::sharded)` → `pub(in crate::sharded)`
(aynı kapsam). Hiçbir alan genişletilmedi. Demo'nun public yolları
aynı (tip takma adları ve yeniden ihraçlar); yeni: `gsb_demo::prelude`,
altı `*Ext` trait'i, `gsb_demo::game::InputAck` artık kit'in tipi.

**Tasarımla çelişen / tasarımın öngörmediği (kayıt):**

1. **`aoi/tests/sharing*.rs` bölündü.** Görev tanımı paylaşım/delta
   testlerini "demo kodeğiyle bayt sabitleyen" grup olarak `gsb-demo`'ya
   gönderiyordu; oysa bu testler ham bayt sabitlemiyor, çözülmüş
   değerlere ve kit'in özel defterine (`book.born_groups`,
   `member_counts`, `pending_removals`, `conn_view`) bakıyor. Özel
   deftere bakan ya da kodeğin değerine bakmayan 6'sı kit'te fikstürle
   kaldı (görünürlük genişletmeden taşınamazlardı); kodeğin değerine
   bakan 6'sı demo'ya gitti. Ölçüt test başına uygulandı (yukarıda).
2. **§12'nin "`Grid3` (27 hücre)"u bir `CellSpace` değil bir `Vision`
   ızgarası:** arena için kurulan ön-ayar `VisionGrid3` adını taşıyor;
   hacimsel AOI `Grid3` tetikleyici bekliyor. §10/§12 metnine not
   düşüldü.
3. **`c97270b` tek başına derlenmiyor:** yeniden adlandırma commit'i
   yalnız `git mv`'leri içeriyor (çalışma ağacındaki yol düzeltmeleri
   bir pathspec hatası yüzünden sahnelenmedi; amend yasak), içerik
   yarısı `10fec02`. İkili birlikte derleniyor ve yeşil; sonraki her
   commit tek başına derleniyor ve yeşil.
4. **`gsb-core`'daki iki yorum** hâlâ `gsb-game` adını anıyor (çekirdek
   dokunulmaz kuralı).

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

### Faz 3 sonucu

**Tamamlandı** (`kit/phase-3-arena`, `addbcf5..`; CHANGELOG "gsb-kit
Faz 3 turu"). Workspace'te yeni `crates/gsb-demo-arena`: küçük bir 3D
takım arenası, görünürlük modeli **3D takım sisi** (bir takım bir
birimi, üyelerinden en az biri ona 3D mesafede görüş yarıçapı içinde
olduğunda görür; yükseklik sayılır), üç takım. **Kabul testi olarak
koştu: `crates/gsb-kit` ve `crates/gsb-core`'a tek satır dokunulmadı**
(`git diff addbcf5.. --stat -- crates/gsb-kit crates/gsb-core` boş;
`gsb-demo` ve `gsb-server` de). Arena `gsb-kit`'e ve `gsb-core`'a bağlı,
`gsb-demo`'ya değil (`cargo tree -p gsb-demo-arena --depth 1`); yalnız
kit'in public yüzeyini görüyor — `pub(crate)` bir şeye erişemediği için
kanıt yapısal (§11.1).

**Arenanın kendisi (oyunun işi, §2):**

| Parça | Seçim | Gerekçe |
|---|---|---|
| Konum | `Pos3 { x, y, z: f32 }`, metre, **y yukarı**; `Spatial` → `[x, y, z]` | `Planar` uygulanmadı: arena hiçbir yer-düzlemi ön-ayarı kullanmıyor, sis bilerek hacimsel |
| Hareket | kendi 3D kinematik hedefe-git sistemi (`Movement`, önbellekli `QueryState`; düz çizgi, sabit hız 12 m/sn, varışta hedefe oturma, duran birime yazmama) | §6: hareket trait'i yok; dikey hareket sıradan hareket (platforma tırmanmak yürümek gibi zaman alır) |
| Codec | `Wire = Cm3` — **tam sayı santimetre, en yakına yuvarlanmış** `i32`; gövde `UnitRecord { uint64 entity = 1; sint32 x, y, z = 2..4 }` | 100 m × 30 m × 100 m arenanın her koordinatı ≤ 2 baytlık zig-zag varint (|v| ≤ 8191); kayıt ≤ 11 bayt (üç `float` ile 17); milimetre çoğu koordinatı 3 bayta iter, desimetre 12 m/sn'lik tırmanmayı tick başına basamaklar; kesme yerine yuvarlama sıfır çevresinde iki kat geniş hücreyi önler |
| Takım ataması | **katılım sırasıyla round-robin** (n. katılım → takım `n mod 3`), takım üssünde spawn | takım boyları en fazla bir farklı; aktarım kimliğinden bağımsız. Elenen: conn modülü (oturum kimlikleri sunucu-geneli; bir odanın katılanları aynı kalanı paylaşabilir), en küçük takımı doldurmak (ayrılış park edildiği için kadro pek değişmiyor; katılım başına dünya taraması) |
| Görüş | `VisionGrid3<Pos3>`, yarıçap **15 m** (zeminin kenarının beşte biri; tavan 30 m'ye karşı yalnız yükseklik farkı da saklayabiliyor) | üsler 25 m'lik halkada, üç takımda 43,3 m ara — taze spawn yalnız kendi takımını görür |
| Oda | `ArenaRoom = TeamRoom<ArenaGame, VisionGrid3<Pos3>>`, kurucu `arena_room(game)` (serbest fonksiyon — kit tipinde inherent impl E0116) | — |
| Bot | park süresi dolan birim, sıradan girdi yolundan (`seq = 0` `MoveTo`) üssüne çekilir | — |
| Wire | `proto/arena.proto` (`gsb.arena`): `MoveTo` (cm, `seq`), `UnitRecord`, kit zarfının tipli aynaları `WorldSnapshot` (`entities = 2` tipli; `cell_exits = 4` aynalanmadı — arenada hücre uzayı yok) ve `Private` (`game = 4` tipli: `Welcome { team, teams }` — oturum başına bir kez, küçük paket G3-3); `gsb.kit.InputAck` olduğu gibi. Build demo'nun kalıbında (`DEP_GSB_KIT_PROTO_DIR`, `.gsb.kit` / `.gsb.base` için `extern_path`) | — |
| Opcode'lar | `ARENA_MOVE_TO = 1100`, `ARENA_SNAPSHOT = 1101`, `ARENA_PRIVATE = 1102` (`Game::SNAPSHOT_OP` / `PRIVATE_OP` ezildi) | oyun bandı; 2D demo'nunkilerden (1000–1006) ayrık blok — iki oyunu tek `MessageTable`'da barındıran bir sunucu yeniden numaralamak zorunda kalmasın |

**Kit'ten kullanılanlar** (tamamı public): `codec::RecordCodec`;
`game::{Game, TeamGame, InputSeq}` (`InputSeq::admit`; birim testinde
`InputSeq::default()`); `team::{TeamRoom, Team}` (`TeamRoom::with_game`;
oda `TeamMember`'ı kendisi yazıyor); `space::{Spatial, VisionGrid3}`
(`VisionGrid3::new`); `proto::{WorldSnapshot, Private, private::Payload,
InputAck}` (`InputAck` arena proto'sunda `extern_path` ile; diğerleri
yalnız wire testlerinde); build tarafında `links = "gsb-kit-proto"`.
Kullanılmayan her şey (diğer odalar, `Planar`, `Grid2`, `CellSpace`,
sharded tipler, `WireId`) gerekmedi.

**Testler** (15: 8 birim + 7 entegrasyon; entegrasyonlar gerçek
`gsb-core` `RoomActor`'ı üzerinden, elle beslenen ticker'la, her
bağlantının kanalına gerçekten düşen kareleri arenanın tipli aynasıyla
çözerek):

| Test | Kilitlediği | Mutation-check (arena tarafında, yedekten geri yüklenerek) |
|---|---|---|
| `fog::each_team_receives_exactly_what_its_members_see` | üç takımın her üyesi tam olarak takımının gördüğünü alıyor — fazlası da eksiği de yok | iki takımlı parite, herkese takım 0, her katılana ayrı takım → kırıldı |
| `fog::team_vision_is_shared_by_members_and_only_by_them` | bir üyenin gördüğü birim takımın uzaktaki üyesine de gidiyor, görmeyen takıma gitmiyor — kendi üyesi yaklaşana dek | aynı üç mutasyon → kırıldı |
| `height::a_unit_straight_above_beyond_the_radius_is_hidden` | aynı zemin noktasında 20 m yukarıdaki düşman görünmüyor, 10 m yukarıdaki görünüyor (düzlem mesafesi 0 — 2D sis bunu geçemez) | `Spatial`'den yükseklik terimi atıldı; `VisionGrid3` yerine `VisionGrid2` (`Planar` `[x, z]`); hareket dikey ekseni yok saydı → kırıldı |
| `height::movement_brings_enemies_into_view_and_takes_them_out` | 25 m'den inen düşman 15 m'ye girdiği İLK tick'te (kayıtta 14,6–15,0 m) pakete giriyor, pakette olduğu her tick'te yarıçap içinde; tırmanınca ilk tick'te çıkıyor; zeminde yürüyen düşman girip çıkıyor | aynı üç mutasyon → kırıldı |
| `input::numbered_inputs_are_acked_and_stale_ones_dropped` | kit'in `InputSeq`'i arenanın `ingest`'i üzerinden: tekdüze yüksek-su ack'i, geç / yinelenen girdi düşüyor ve birimi eski hedefe döndürmüyor, `seq = 0` uygulanıp ack'lenmiyor | sıra kuralını yok sayan `ingest` → kırıldı |
| `wire::kit_envelope_and_arena_mirror_encode_identically` | kurulmuş kareler (full, delta, ack + yanıt, one-shot full) iki tanımda aynı bayt, birbirini kendine çözüyor | aynanın `entities = 2 → 6` → kırıldı |
| `wire::real_room_frames_decode_identically_through_both_definitions` | gerçek odanın yazdığı takım paketi ve ack karesi `gsb_kit::proto` ve aynadan aynı içeriğe çözülüyor, ikisi de **kit'in elle yazdığı baytların aynısına** yeniden kodlanıyor; kayıtlar santimetre | aynı mutasyon ve dikey hareketsizlik → kırıldı |

Birim testleri: nicemleme (yuvarlama, simetri, doygunluk), kayıt gövdesi
= tipli `UnitRecord`, katılım sırasıyla round-robin (hepsi çift yedi
conn → 0,1,2,0,1,2,0), üslerin birbirinin görüşü dışında olması (3–5
takım), bot'un girdi yolundan üsse çekilmesi, 3D doğru boyunca hareket
ve varış, duran birime yazılmaması, opcode bandı.

**Tasarım bulguları.** Kit'in public API'si arenayı **engellemedi**:
kit'e değişiklik gerekmedi, hiçbir kit iç öğesi kopyalanmadı, hiçbir
şey etrafından dolaşılmadı. Karşılaşılan iki pürüz (ikisi de
engelleyici değil, kayıt):

1. **Takım ataması spawn'dan SONRA soruluyor.** Oda önce
   `Game::spawn_player`'ı, hemen ardından `TeamGame::team_of`'u
   çağırıyor (`common::join` → `TeamRoom::on_join`). Takım oyununda
   spawn noktası takıma bağlı (üs), yani karar spawn'da verilmek
   zorunda: arena takımı `spawn_player`'da seçip birime `HomeBase`
   olarak yazıyor, `team_of` yalnız onu geri okuyor. Sonuç: `team_of`
   pratikte bir okuyucu, takım bilgisi de dünyada iki bileşende
   (`HomeBase` + kit'in `TeamMember`'ı). Çalışıyor, çünkü kit iki
   çağrının sırasını ve aynı katılıma ait olduklarını belgeliyor
   (`team_of` "the player whose entity `spawn_player` just spawned").
   **En küçük kit değişikliği (yalnız ekleme):** `TeamGame`'e
   varsayılanlı bir kanca — `fn spawn_team_player(&mut self, world:
   &mut World, conn) -> (Entity, Team)`, varsayılanı bugünkü sıra
   (`spawn_player`, ardından `team_of`) — ve `TeamRoom::on_join`'in
   katılımda onu çağırması (kit-içi: `common::join`'e spawn adımını
   parametre olarak geçmek). Mevcut oyunlar (demo'nun conn paritesi)
   değişmeden derlenir; takıma bağlı spawn isteyen oyun kancayı ezip
   takımı ve üssü tek yerde seçer. Tetikleyici: takıma bağlı spawn
   isteyen ikinci bir oyun (Faz 4'ün MMO'su takım sisi kullanmıyor).
2. **Kit zarfının istemci kuralları demo'nun proto'sunda yazılı.**
   `kit.proto`'nun `WorldSnapshot` yorumu delta/full istemci
   kurallarını (baseline, sıra boşluğu, yinelenen kare, keep-alive
   yakınsaması) "kit'in kuralları, her oyun için geçerli" diye anıyor
   ama metni `gsb-demo/proto/game.proto`'ya yönlendiriyor. İkinci bir
   oyunun istemci yazarı kit'in sözleşmesini öğrenmek için diğer
   örnek oyunun proto'sunu okumak zorunda — bir belge bağlılığı (kod
   değil). Arena kendi aynasına yalnız kendi odasının (takım sisi:
   yalnız full) kuralını yazdı. **En küçük kit değişikliği:** yalnız
   yorum — istemci kurallarını `kit.proto`'ya taşımak (demo'nun aynası
   oraya atıf yapar).

**Gözlemler (bulgu değil — arena bunlara takılmadı):**
- `Game::SNAPSHOT_OP` / `PRIVATE_OP`'un kit'teki varsayılanları 2D
  demo'nun numaraları (1003 / 1004). Arena ezdi; ezmeyi unutan bir oyun
  demo'yla aynı opcode'u kullanır. Varsayılansız ilişkili sabit (derleme
  hatasıyla zorunlu kılmak) daha dürüst olurdu; tetikleyici yok.
- `Vision::sees(viewer, target)` yalnız iki konumu alıyor: birim başına
  görüş yarıçapı (MOBA'nın ward / kahraman farkı) ancak yarıçapı konum
  tipine gömen kendi `Vision`'ıyla yazılabilir. Arena bilinçli olarak
  tekdüze yarıçap seçti.
- `TeamRoom` yalnız full gönderir; hızlı hareket eden bir arenada her
  tick her takıma bir full demek. Stratejinin tasarımı (§10 "Faz 1b"),
  ölçülmedi, tetikleyicisiz iş yapılmaz. → **T turunda** (BACKLOG §1
  satır 5): `TeamRoom::with_delta`, arena onu kullanıyor — aşağıda
  "T sonucu" (ölçüm: arena botunda kazanç ~%10, çünkü görünür
  birimlerin ~%90'ı her tick hareket ediyor).

**Doğrulama:** 439 + 15 = **454** test / 0 hata / 1 ignored; `cargo
clippy --workspace --all-targets -- -D warnings` 0 uyarı; loadgen
(50 istemci, 3 sn) `left=50 errors=0` — arena sunucuya ve loadgen'e
bağlı değil (§12 kapsam dışı), çalışma zamanında değişen kod yok; koşu
yalnız workspace'in sağlam kaldığının kanıtı (A/B alınmadı).

### Faz 4 sonucu

**Tamamlandı** (`kit/phase-4-mmo`, `89049d2..`; CHANGELOG "gsb-kit Faz 4
turu"). Workspace'te yeni `crates/gsb-demo-mmo`: küçük bir 3D MMO
dünyası, görünürlük modeli **shard'lı bir dünya üzerinde uzamsal ızgara
AOI** — kit'in `sharded × spatial` kompoziti
(`ShardedSpatialRoom<MmoGame, GridPartition2<Pos3>, Grid2>`), 2D
ön-ayarlar 3D verinin **yer düzleminde** (`Planar` = `[x, z]`).
Konumlar 3D (metre, y yukarı; uçan mob'lar 200 m'ye kadar), ilgi
yönetimi yüksekliği bilerek yok sayıyor — arenanın hacimsel sisinin
tersi. **Kabul testi olarak koştu: beş korunan crate'e tek satır
dokunulmadı** (`git diff 89049d2.. --stat -- crates/gsb-kit
crates/gsb-core crates/gsb-demo crates/gsb-demo-arena crates/gsb-server`
boş; `git log addbcf5.. -- crates/gsb-kit` boş — kit Faz 2'den beri
aynı). MMO `gsb-kit` + `gsb-core` + `gsb-protocol`'e bağlı, iki demoya
da değil (`cargo tree -p gsb-demo-mmo --depth 1`); yalnız kit'in public
yüzeyini görüyor. Kit'te değişiklik gerektiren her ihtiyaç aşağıda
**tasarım bulgusu** olarak kayıtlı — kit yamanmadı, iç öğe
kopyalanmadı, hiçbir bulgunun etrafından sessizce dolaşılmadı.

**MMO'nun kendisi (oyunun işi, §2):**

| Parça | Seçim | Gerekçe |
|---|---|---|
| Konum | `Pos3 { x, y, z: f32 }`, metre, **y yukarı**; `Planar` → `[x, z]` | ilgi ve sharding yer düzleminde; `Spatial` uygulanmadı (hiçbir 3D ön-ayar kullanılmıyor) |
| Dünya | 1 024 m × 1 024 m zemin (`WORLD_HALF = 512`), 200 m gök | — |
| Bölme | `GridPartition2::new(4, 512)`: 2×2 shard, 512 m bölgeler, ön-ayarın şeridi 128 m (bölge kenarının dörtte biri) | N ≥ 4 (görev); dikişler x = 0 ve z = 0'da |
| AOI | `Grid2::new(64)`: 64 m hücre, 3×3 görünüm (64–128 m) | dikişler hücre kenarına düşüyor; bir görünüm dikişin öbür yanına en çok bir hücre (64 m) uzanıyor — 128 m'lik şeridin içinde |
| Codec | `Wire = MmoWire { x, y, z: i32 dm, kind, hp }` — **en yakına yuvarlanmış desimetre** + tür + can; `Dirty = Or<(Changed<Pos3>, Changed<Vitals>)>`; gövde `EntityRecord { entity = 1; x, y, z = 2..4; Kind kind = 5; uint32 hp = 6 }` | ±5 120 dm zemin, 0..2 000 dm yükseklik: her koordinat ≤ 2 baytlık zig-zag varint (üç koordinat etiketleriyle ≤ 9 bayt, üç `float` 15); santimetre ±81,9 m ötesini 3 bayta iter. 7 m/sn koşan oyuncu 30 Hz'de tick başına 23 cm — her tick bir kayıt; 1 m/sn'lik mob üç tickte bir; duran varlık bedava. Kesme yerine yuvarlama: dikişlerdeki sıfır hücresi iki kat geniş olmasın |
| Wire'ın `Planar`'ı | `[x.div_euclid(10), z.div_euclid(10)]` — **metre** (konumun birimi), desimetre değil | ön-ayarlar tek dünya biriminde çalışsın (`Grid2`'nin hücre kenarı metre; `GridPartition2::admits` şerit kaydını bölge dikdörtgeniyle konumun biriminde karşılaştırıyor — bulgu F3); hücre yine `floor(dm / 640)` (istemcinin kuralı, birim testiyle `Grid2`'ye sabit) |
| Hareket | oyuncular hedefe koşuyor (7 m/sn, zeminde); mob'lar rotalarında **kendi hızlarıyla** (`Mob` beyni; `Speed` benzeri bileşen YOK — §8.5'i doğrudan sınar); uçanlar irtifalarını koruyor | §6 |
| NPC yaşam döngüsü | **oyun kodu:** spawn tablosunun kampları (`MobSpawn`: nokta, rota, hız, can, ilk tick, periyot, ömür) her shard'da yalnız kendi zeminindekileri spawn ediyor; kit damgalıyor (`Marker` = yayın); ömrü dolan mob `World::despawn`; canı sıfırlanan mob (`Attack`) `World::despawn` | §8.2'yi (hayalet) ve §8.5'i (göç) doğrudan sınar |
| `Mig` | `MmoMig::Player { pos, vitals, speed, target }` / `MmoMig::Mob { pos, vitals, mob }` — mob'un bütün beyni (rota, bacak, hız, ölüm tick'i) | sınırı geçen oyuncu koşmaya, mob rotasına devam ediyor ve planlandığı tick'te yeni shard'da ölüyor |
| Girdi | `MoveTo { x, z, seq }` (dm), `Attack { target, seq }` (yalnız kendi shard'ındaki, 30 m içindeki mob), `Travel { waystone, seq }` (anında ışınlanma, yürüyüşü iptal eder) — hepsi tek sıra uzayında, kit'in `InputSeq`'i | — |
| Karakter kaydı | `Realm.logins`: kaydedilmiş karakter konumu, **oturum** kimliğiyle; join yönlendirmesi kayıtlı konumun shard'ı (`world::home_shard`) | `spawn_player` yalnız taşıma oturumunu alıyor (gözlem, aşağıda). *K4'ten beri doğrulanmış oyuncu kimliğiyle (`spawn_player_as`) — GAME-MODULE "K4 sonucu"* |
| Park politikası | kit'in park defteri: bağlantı düşünce karakter **bekletiliyor** (dünyada, aynı wire id, slot tutulu; `LOGOUT_GRACE = 20 sn`); süre dolunca kit **AI devrine** veriyor — MMO'nun botu karakteri en yakın waystone'a (güvenli nokta) yürütüyor; `grace = 0` hemen bırakıyor | "süre dolunca slotu bırak" (MMO'nun olağan çıkış sayacı) ifade edilemiyor — bulgu F4 |
| Oda | `MmoShard` takma adı; kurucu `mmo_shard(index, &realm)` (serbest fonksiyon — kit tipinde inherent impl E0116) | — |
| Wire | `proto/mmo.proto` (`gsb.mmo`): girdiler, `Kind` enum'u, `EntityRecord`, yer-düzlemi `CellExit { sint32 x = 1; sint32 z = 2; }` (`Grid2`'nin gövdesiyle bayt bayt aynı), kit zarfının tipli aynaları `WorldSnapshot` (`entities = 2`, `removed = 3`, `cell_exits = 4`, `delta = 5`) ve `Private` (`game = 4` aynalanmadı); `gsb.kit.InputAck` olduğu gibi; kit proto'su `links` (`DEP_GSB_KIT_PROTO_DIR`) üzerinden | — |
| Opcode'lar | `MMO_MOVE_TO = 1200`, `MMO_SNAPSHOT = 1201`, `MMO_PRIVATE = 1202`, `MMO_ATTACK = 1203`, `MMO_TRAVEL = 1204` | 2D demo (1000–1006) ve arenadan (1100–1102) ayrık blok |

**Kit'ten kullanılanlar** (tamamı public): `codec::RecordCodec`;
`game::{Game, ShardGame, InputSeq}` (`InputSeq::admit`; birim testinde
`InputSeq::default()`); `sharded::{ShardedRoom, ShardedSpatialRoom,
KitMig}` (`ShardedRoom::with_game`, `ShardedSpatialRoom::with_shard`,
`with_disconnect_grace`; `KitMig` yalnız test koşumunun mesaj tipinde);
`space::{Planar, Grid2, GridPartition2, shard_at, CellSpace, Cell,
Partition}` (`CellSpace`/`Cell`/`Partition` yalnız birim testlerinde:
istemci hücresinin ve bölge filtresinin kit'le aynı olduğunu
sabitlemek için); `identity::WireId` (`get()` — `Attack` hedefini
çözmek için); `proto::{WorldSnapshot, Private, private::Payload,
InputAck}` (`InputAck` MMO proto'sunda `extern_path` ile; diğerleri
yalnız wire testlerinde); build tarafında `links = "gsb-kit-proto"`.
Kullanılmayan her şey (diğer odalar, `Vision`/`SectorMap` ön-ayarları,
`Spatial`, `TeamGame`) gerekmedi.

**Testler** (24: 12 birim + 12 entegrasyon). **Entegrasyonların
hepsi gerçek `gsb-core` shard aktörleri üzerinden:** dört `ShardActor`,
registry'nin kablolamasıyla (komşu yuvalarında komşuların posta
kutuları, gerisinde kukla), tek elle beslenen ticker; adım bariyeri
metrik kanalı (`metrics_cadence_hz == tick_hz`: her shard yayın
fazından SONRA tick başına bir örnek); istemciler kendi kanallarına
gerçekten düşen kareleri MMO'nun tipli aynasıyla, kit'in istemci
kurallarıyla (full / delta / `cell_exits` / one-shot private full)
çözüyor ve her batch'te akış değişmezlerini (tick başına en çok bir
snapshot + bir private, hiçbir karede tekrar eden kimlik) doğruluyor.
Birim testleri odayı doğrudan sürüyor (`game::tests`'in biri —
`attack_and_travel…` — MMO'nun gerçek kit odasını `GameLogic`
metotlarıyla; kalanları oyun kancalarını, kodeği ve kit ön-ayarlarını
doğrudan): onlar oda davranışı değil oyun mantığı ve wire sabitlemesi
sınıyor, aktör gerekmiyor.

| Test | Kilitlediği | Mutation-check (MMO tarafında, yedekten geri yüklenerek) |
|---|---|---|
| `aoi::a_player_sees_exactly_its_ground_cell_block_whatever_the_height` | iki oyuncunun her biri 40 tick boyunca (iki keep-alive full dahil) TAM OLARAK 3×3 yer hücresi bloğundakileri alıyor — komşu shard'ın şeridinden ödünç gelen dahil; komşu hücrede 150 m yukarıdaki uçan görünür, iki hücre ötede zemindeki mob görünmez, oysa uçan 3D'de daha uzak | `Planar` `[x, y]` (konum + wire) → kırıldı (tick 6: uçanlar görünmüyor) |
| `crossing::a_player_crossing_a_seam_keeps_its_id_state_and_stream` | x = 0 dikişini koşarak geçen oyuncu: oturum shard 1'e taşınıyor, wire id + can aynı, taşınan yürüyüşle hedefe varıyor, hep ileri gidiyor; iki yandaki gözlemciler onu her tick görüyor (şerit önce ve sonra); kendini kaybetmesine YALNIZ varış tick'inde izin var — F1 | `Mig` hedefi taşımıyor → kırıldı ("B lost P at tick 150") |
| `crossing::a_mob_crossing_a_seam_keeps_its_id_and_its_brain` | shard 0'ın kampından doğan, oyuncusu ve hız bileşeni olmayan uçan mob dikişi geçiyor: B her tick tam bir uçan ve aynı wire id görüyor; irtifa, shard 0'da aldığı hasar, rotanın ikinci bacağı ve shard 0'ın planladığı ölüm tick'i (605) yeni shard'da geçerli | `capture` mob'u oyuncu sayıyor (beyin taşınmıyor) → kırıldı |
| `crossing::a_teleport_into_a_non_adjacent_shard_lands_once_with_the_same_id` | köşegen shard'ın waystone'una ışınlanan oyuncu: oturum hiçbir tick iki shard'da değil, iki tick yolda (iki adım — ara shard kurup AYNI tick'te iletiyor, §8.4), shard 3'e bir kez iniyor, iptal edilen yürüyüş geri gelmiyor, aynı wire id | `Travel` yürüyüşü iptal etmiyor → kırıldı |
| `despawn::a_mob_killed_by_game_code_vanishes_everywhere_for_good` | oyun kodunun öldürdüğü mob (`World::despawn`) hem kendi shard'ındaki hem şeritten gören komşu shard'daki istemciden kalkıyor; dört keep-alive full boyunca hiçbir karede geri gelmiyor; yalnız canı değişen mob (konum aynı) iki yana da haber | öldürme despawn etmiyor → kırıldı; `Dirty` yalnız konum → kırıldı |
| `reconnect::a_parked_character_stays_and_resumes_with_its_wire_id` | park: karakter dünyada kalıyor, slot tutulu (`members 2, detached 1`); yayın resume'u tam bir shard kabul ediyor, aynı wire id, numaralı girdi yeniden işliyor | — (park defteri kit'in; MMO yalnız politikayı seçiyor) |
| `reconnect::grace_expiry_hands_the_character_to_the_logout_bot` | süre dolunca AI devri (`detach_expired_ai 1`), karakter canlı, bot onu en yakın waystone'a yürütüyor; **F4 kilidi:** `detach_expired_despawn 0`, slot tutulu | bot sessiz → kırıldı |
| `reconnect::zero_grace_releases_the_slot_at_once` | `grace = 0`: slot hemen bırakılıyor, karakter görünümden çıkıyor, resume'u kimse kabul etmiyor | — |
| `wire::kit_envelope_and_mmo_mirror_encode_identically` | kurulmuş kareler (full, `removed` + `cell_exits` + kayıtlı delta, ack + yanıt, one-shot full) iki tanımda aynı bayt, birbirini kendine çözüyor | aynada `entities = 6` → kırıldı |
| `wire::real_shard_frames_decode_identically_through_both_definitions` | gerçek shard karelerinin hepsi (grup full'u, `removed`'lı delta, `cell_exits`'li delta — hücre (2,4), one-shot private full, ack) iki tanımdan aynı içeriğe çözülüyor ve aynı baytlara yeniden kodlanıyor; full ve private'lar kit'in elle yazdığı baytların TA KENDİSİNE (delta'lar olamaz: kit alanları istemcinin uygulama sırasıyla ve `removed`'ı packed olmadan yazıyor, üretilmiş kodlayıcı alan numarası sırasıyla — her ayrıştırıcı için aynı mesaj) | aynı mutasyon → kırıldı |
| `findings::f1_…`, `findings::f2_…` | F1 ve F2'yi BUGÜNKÜ davranışla sabitliyor (aşağıda) — kit düzeltilince bilerek kırılır, çevrilir | kit-kopyası probları (aşağıda) |

Birim testleri: nicemleme (yuvarlama, simetri, doygunluk), kayıt gövdesi
= tipli `EntityRecord`, `Grid2`'nin wire üzerindeki hücresi = istemcinin
`floor(dm / 640)`'ı (yükseklikten bağımsız), `CellExit` gövdesi = tipli
ayna, shard ızgarasının wire'ı metre okuması (F3; mutasyon: wire
`Planar`'ı desimetre → kırıldı), kayıtlı konumda spawn, kampların
kendi zemininde spawn / periyot / ömür, kit odası üzerinden `Attack` +
`Travel`, çıkış botu, `capture`/`restore` gidiş-dönüşü, canlı spawn
tablosunun dört shard'ı da kapsaması, opcode bloğu.

**Üç demo birlikte — kapanış kontrolü** (aynı kit, Faz 2'den beri
değişmemiş: `git log --oneline addbcf5..HEAD -- crates/gsb-kit` boş):
`cargo test -p gsb-demo -p gsb-demo-arena -p gsb-demo-mmo` → **82
passed / 0 failed** (2D demo 43, arena 15, MMO 24). Üç görünürlük
modeli — 2D'de bütün stratejiler + bayt uyumluluğu, arenada 3D takım
sisi (`Spatial` + `VisionGrid3`), MMO'da yer-düzlemi ızgara AOI +
sharding (`Planar` + `Grid2` + `GridPartition2`) — tek kit üzerinde.

**Sunucu kancası değerlendirmesi (§12 son paragraf).** MMO'yu uçtan
uca çalıştırmak için sunucunun istediği üç şey MMO'da hazır ve public:
shard başına mantık (`mmo_shard(i, &realm)`), mesaj tablosu
(`gsb_demo_mmo::register`), join yönlendirmesi (`world::home_shard`,
kayıtlı karakterin konumundan). Eksik olan sunucu tarafında: fabrika ve
`build_table` 2D demo'ya bağlı (§9) — en küçük kanca, fabrika seçimini
bir "oyun modülü"ne (fabrika fonksiyonu + `register` + `home_shard`)
açmak; §9'un kapsam dışı bıraktığı iş, ayrı tur. Ayrıca join
yönlendiricisi de `spawn_player` gibi yalnız oturum kimliğini görüyor
(gözlem 1; *K4 turunda kapandı — yönlendirici `(conn, kimlik)` alıyor*).

**Tasarım bulguları.** Kit'in public API'si MMO'yu **inşa etmeyi
engellemedi** — hiçbir kit değişikliği yapılmadan her parça kuruldu ve
kabul testleri yeşil — ama dört yerde MMO'nun ihtiyacı kit'in
bugünkü davranışının dışında kaldı; biri (F1) bir **doğruluk
hatası**. Her biri için kanıt, gerekçe ve en küçük kit değişikliği:

1. **F1 — Sharded × spatial: kendi hücresinde ödünç verilmiş bir
   entity o hücreye göç edince yeni shard'ında siliniyor.** (Doğruluk
   hatası; 2D demo'nun `sharded × spatial`'ında da aynı kod yolu.)
   Bir entity dikişi geçerken alıcı shard onu zaten şeritten ödünç
   tutuyordur (dikişe yakın her şey ihraç edilir). Göçün tick'inde
   alıcının dirty pass'i OWN kaydı hücresine koyuyor
   (`record_appearance`); ardından yayın fazında
   `ShardedSpatialRoom::integrate_borrowed` entity'yi şeritte bulamıyor
   (çekirdeğin own-wins filtresi onu düşürdü) ve ÖNCEKİ ödünç
   konumunun hücresinden `record_exit` ediyor — own kaydın az önce
   girdiği kovanın ta kendisinden. Modül belgesinin "the ledger simply
   never saw that id" varsayımı yanlış: defter o kimliği her zaman
   görmüştür. Sonuçlar (hepsi testle görüldü): varış tick'inin one-shot
   full'u gelen oyuncunun kendisini içermiyor (`crossing` testinde
   oyuncu varış tick'inde — 136 — kendini kaybediyor; bir sonraki
   delta geri getiriyor); entity hücresinde YALNIZSA kova tamamen
   gidiyor, hücre "exited" sınıflanıyor, o hücreyi gören herkes onu
   `cell_exits` ile unutuyor ve entity o hücrede kıpırdamadıkça
   (`record_update` var olmayan kovaya eklemiyor) hiçbir full'da, hiçbir
   yeni katılanın görünümünde yok — yani **yeni shard'ında süresiz
   görünmez**, oysa dikişin öbür yanındaki shard onu şeritten görmeye
   devam ediyor (`findings::f1_…`: dikişi santimlerle geçip duran
   mob; ayrıca köşegen ışınlanmada waystone'da duran oyuncu — `crossing`
   testi bu yüzden inişten sonra onu bir hücre öteye yürütüp
   doğruluyor). **En küçük kit değişikliği** (kit-içi, API yok, ~6
   satır): `integrate_borrowed`'ın çıkış döngüsünde, şeritten düşen
   kimlik artık bu shard'ın kendi entity'siyse
   (`inner.wire_entity.get(wire)`) ve dirty pass onu aynı hücreye
   koyduysa (`book.cell_of_entity(e) == Some(c)`) `record_exit`'i atla;
   başka hücredeki bayat ödünç kopya yine çıkarılır. **Prob** (yalnız
   çalışma alanının scratchpad kopyasında, bu worktree'de değil): bu
   değişiklikle `gsb-kit` + `gsb-demo` + `gsb-demo-mmo` testlerinin
   125'i geçti, 1'i kırıldı — bilerek sabitlenen `findings::f1_…`
   (çevrilmesi gereken); `crossing`'deki kayıp-kendi tick'leri
   `[136]` → `[]`.
2. **F2 — `GridPartition2` 4-komşuluk: köşeden ödünç yok.** Şerit
   yalnız komşular arasında değiş tokuş ediliyor, ön-ayarın komşuları
   batı/doğu/kuzey/güney. Bir bölge köşesine yakın oyuncunun 3×3'ü
   köşegen shard'a bir hücre uzanıyor, ama köşegen shard ona hiçbir şey
   ödünç vermiyor: harita merkezine 10 m'deki oyuncu iki kenar
   komşusunun mob'larını görüyor, köşegendekini (aynı uzaklıkta)
   görmüyor (`findings::f2_…`). MMO kendi `Partition`'ını yazarak
   (public seam) dolaşabilirdi — yapılmadı: turun konusu ön-ayar ve bu
   tam da sessiz bir etrafından dolaşma olurdu. **En küçük kit
   değişikliği** (yalnız ekleme): `GridPartition2::with_diagonals()` —
   köşegenleri de komşu sayan 8-komşuluk bayrağı, varsayılan
   değişmeden. **Prob:** varsayılanı 8-komşuluğa çevirmek köşe testini
   çeviriyor ama iki kit yönlendirme testini (4-komşuluk rotalarını
   sabitleyen) ve MMO'nun "iki adım" ışınlanma iddiasını kırıyor
   (köşegen artık tek adım) — bu yüzden bayrak, varsayılan değişikliği
   değil.
3. **F3 — `Planar`'ın birim sözleşmesi yazılı değil.**
   `GridPartition2::admits` wire değerinin `Planar`'ını bölge
   dikdörtgeniyle konumun biriminde karşılaştırıyor; wire'ı konumdan
   ince nicemleyen bir oyun (santimetre, desimetre) `[x_dm, z_dm]`
   yazarsa derlenir ve düz `ShardedRoom`'da şerit sessizce yanlış
   süzülür (128 m'lik kenar 12,8 m olur). MMO wire `Planar`'ını metreye
   kabalaştırarak yaşıyor (hücre kenarı tam metre olduğu için bedelsiz;
   birim testi + mutasyon kilitli). Engelleyici değil. **En küçük kit
   değişikliği:** yalnız belge — `Planar` ve `GridPartition2`
   belgelerine "wire izdüşümü konumun biriminde olmalı" (ya da,
   tetikleyici çıkarsa, ön-ayara bir `wire_scale`). Yan gözlem: MMO'nun
   odası (`ShardedSpatialRoom`) `admits`'i hiç çağırmıyor — komşunun
   bütün ihracını deftere alıyor; görünürlük hücreyle sınırlı olduğu
   için doğru, ama uzak ihraç kayıtları (ör. ışınlanan bir oyuncunun
   bölge dışındaki konumu) ara shard'ların defterine giriyor.
4. **F4 — Park politikası her beklemeyi AI devrine bitiriyor.** Kit
   `on_disconnect`'e her zaman `Detach::Hold { grace: Some(grace), to:
   ExpireTo::AiHandover }` cevabı veriyor (`common::park`,
   `ParkPolicy { grace }`); oyun `ExpireTo`'yu seçemiyor. MMO'nun olağan
   çıkış sayacı — "karakter süre boyunca dünyada kalsın, sonra slotu
   bıraksın" (`ExpireTo::Despawn`) — ifade edilemiyor; savaşta çıkışı
   geciktiren `may_release` vetosu da (`grace: None`) erişilemez.
   Çekirdek ve kit'in defteri ikisini de zaten destekliyor
   (`park_on_expire`'ın `Despawn` kolu var, hiçbir kit yolu onu
   seçmiyor). MMO AI devriyle yaşıyor (bot karakteri güvenli noktaya
   yürütüyor, slot tutulu) ve `reconnect` testi sınırı kilitliyor.
   **En küçük kit değişikliği** (yalnız ekleme): `ParkPolicy`'ye `to:
   ExpireTo` (varsayılan `AiHandover`) ve odalara
   `with_disconnect_policy(grace, to)` kurucusu; kişi başına karar
   isteyen oyun için alternatif `Game::disconnect_policy(&self, world,
   entity) -> (Option<Duration>, ExpireTo)`, varsayılanı bugünkü cevap.
   **Prob:** kopyada `to` `Despawn`'a çevrilince çekirdeğin `Despawn`
   yolu kit defteriyle devreye giriyor (`detach_expired_ai` 1 → 0).

**Gözlemler (kit bulgusu değil):**
1. `spawn_player` — ve join yönlendiricisi — yalnız taşıma oturumunu
   (`ConnectionId`) görüyor; hesap kimliği çekirdeğin `on_join(world,
   conn)`'unda kalıyor (`ShardMsg::Join` onu taşıyor ama `GameLogic`'e
   vermiyor). MMO kayıtlı karakterleri oturumla anahtarlıyor (sunucunun
   login adımı doldururdu). Kit, çekirdeğin vermediğini veremez —
   düzeltme çekirdekte (`on_join`'e kimlik), kapsam dışı. **Kapandı —
   K4 turu** (GAME-MODULE "K4 sonucu"): çekirdek `home_shard`'a ve
   `GameLogic::on_join_as`'a doğrulanmış kimliği veriyor, kit onu
   `Game::spawn_player_as`'a iletiyor, MMO realm'i onunla anahtarlı.
2. Şeritten görünen (komşunun sahip olduğu) mob'a saldırı yok:
   CROSS-SHARD §2'nin `RemoteEffect`'i uygulanmadı (çekirdek). MMO
   saldırıyı saldıranın shard'ında çözüyor.
3. Eski yanda göç anında kırpışma ölçülmedi değil, **görülmedi**:
   `crossing` testinde iki yandaki gözlemciler geçen oyuncuyu her tick
   gördü.
4. `Game::SNAPSHOT_OP` / `PRIVATE_OP` varsayılanları — MMO da ezdi
   (Faz 3'ün gözlemi geçerli).

### Faz 3 + Faz 4 tasarım bulguları — kit düzeltme turunun girdisi

Kapanış doğrulamasının iki turunda (arena, MMO) kit'e tek satır
dokunulmadan kaydedilen her bulgu, en küçük kit değişikliğiyle (F =
Faz 4, yukarıda; A = Faz 3'ün arena bulguları, "Faz 3 sonucu" 1 ve 2):

| # | Bulgu | Tür | En küçük kit değişikliği | Durum |
|---|---|---|---|---|
| F1 | Sharded × spatial: kendi ödünç hücresine göç eden entity yeni shard'ının kovasından siliniyor (varış full'unda kendini kaybetme; hücresinde yalnızsa süresiz görünmezlik) | **doğruluk hatası** | `ShardedSpatialRoom::integrate_borrowed`: artık own olan ve dirty pass'in aynı hücreye koyduğu kimlik için `record_exit`'i atla (kit-içi, ~6 satır); `findings::f1_…` çevrilir | **çözüldü** — Faz 5, `c8ed9ba` |
| F2 | `GridPartition2` 4-komşuluk: köşegen shard köşeden hiçbir şey ödünç vermiyor | eksik özellik (MMO görünürlüğü) | `GridPartition2::with_diagonals()` — 8-komşuluk bayrağı, varsayılan aynı (ekleme); `findings::f2_…` çevrilir | **çözüldü** — Faz 5, `fc4d10b` |
| F4 | Park politikası her beklemeyi AI devrine bitiriyor; "sonra bırak" ve savaş vetosu ifade edilemiyor | eksik politika | `ParkPolicy.to: ExpireTo` + `with_disconnect_policy(grace, to)` (ekleme); ya da varsayılanlı `Game::disconnect_policy` | **çözüldü** — Faz 5, `cfc51dc` |
| A1 | Takım, spawn'dan SONRA soruluyor (takıma bağlı spawn noktası `spawn_player`'da seçilip geri okunuyor) | seam sırası | `TeamGame::spawn_team_player(&mut self, world, conn) -> (Entity, Team)`, varsayılanı bugünkü sıra (ekleme) | **çözüldü** — Faz 5, `c076fd0` |
| F3 | `Planar`'ın birim sözleşmesi yazılı değil (`GridPartition2::admits` wire'ı konumun biriminde okuyor) | belge | `Planar` / `GridPartition2` belgesine bir cümle (tetikleyiciyle `wire_scale`) | **çözüldü** — Faz 5, `deb1d4e` (belge + debug denetimi) |
| A2 | Kit zarfının istemci kuralları `kit.proto`'da değil demo'nun `game.proto`'sunda | belge | kuralları `kit.proto`'ya taşı, demo'nun aynası atıf yapsın (yalnız yorum) | **çözüldü** — Faz 5, `7356bd7` |
| A3 | Kit'in `Game::SNAPSHOT_OP` / `PRIVATE_OP` varsayılanları 2D demo'nun numaraları (gözlem, aşağıda) | API dürüstlüğü | varsayılansız ilişkili sabit (oyun yazarı için kırıcı) | **çözüldü** — Faz 5, `2d39767` |

**Hepsi Faz 5'te çözüldü** (§10 "Faz 5 sonucu"; A3 bu tabloya Faz 5'te
eklendi — aşağıdaki gözlemlerin ilki).

**Sonradan (GAME-MODULE G2, gerçek sunucu altında) bulunan kit
bulguları:** K1 (göçü tetikleyen girdi hiç ack'lenmiyor), K2 (sıra kuralı
göçte sıfırlanıyor), K3 (kaynakta `InputSeq` girdisi sızıyor) — üçü de
`4d83d01`'de **çözüldü**: `KitMig.input: Option<ShardInputRecord { hwm,
acked }>`; `collect_migrations` okur, `on_migrate_out` siler,
`on_migrate_in` kurar (`InputSeq::adopt`); iki sharded odada da (spatial
kompozit delege ediyor). Kanıt: `sharded/tests/input_carry.rs` (7, önce
kırıldı) ve çevrilen `gsb-server` `mmo_findings.rs`; ayrıntı
GAME-MODULE §5 "Kit düzeltme turu".

Tetikleyicisiz gözlemler (iş yok): ~~kit'in opcode varsayılanları
(varsayılansız ilişkili sabit daha dürüst olurdu)~~ (kapandı — A3,
`2d39767`: `Game::SNAPSHOT_OP` / `PRIVATE_OP` artık zorunlu), `Vision::sees`
birim başına yarıçap taşımıyor, ~~`TeamRoom` yalnız full gönderiyor~~
(T turu: `with_delta`, §10 "T sonucu"),
`ShardedSpatialRoom` `admits`'i uygulamıyor (F3'ün yan gözlemi —
sonradan kapandı, "Faz 5 sonucu" gözlemleri),
join / `spawn_player` hesap kimliğini görmüyor (çekirdek). Sıra
önerisi: F1 (hata) önce ve kendi commit'inde, önce kırılan testiyle
(`findings::f1_…` zaten hazır); ardından eklemeler (F2, F4,
A1); belgeler (F3, A2) aynı turda.

**Doğrulama:** 454 + 24 = **478** test / 0 hata / 1 ignored; `cargo
clippy --workspace --all-targets -- -D warnings` 0 uyarı; loadgen (50
istemci, 3 sn) `left=50 errors=0` — MMO sunucuya ve loadgen'e bağlı
değil (§12 kapsam dışı), çalışma zamanında değişen kod yok; koşu yalnız
workspace'in sağlam kaldığının kanıtı (A/B alınmadı).

### Faz 5 sonucu

**Tamamlandı** (`kit/phase-5-findings`, `32118b2..`; CHANGELOG "gsb-kit
Faz 5 turu"). Kit düzeltme turu: iki kontrol demosunun "kit'e dokunma"
kuralıyla kaydettiği her bulgu (yukarıdaki tablo) kit'te kapandı, her
biri kendi commit'inde ve kendi kanıtlayan testiyle; demolar düzeltmeyi
kullanacak şekilde güncellendi, bulguları bugünkü (hatalı) davranışla
sabitleyen testler çevrildi. **`gsb-core`'a dokunulmadı** (`git diff
32118b2.. --stat -- crates/gsb-core` boş); wire baytları değişmedi
(`gsb-demo`'nun `wire_contract.rs`, `kit_wire.rs`, `delta_aoi.rs`, kit'in
`aoi/tests/sharing*`, arenanın ve MMO'nun `wire.rs`'i dokunulmadan
yeşil — `git diff 32118b2.. --stat -- crates/gsb-demo/tests
crates/gsb-demo-arena/tests crates/gsb-demo-mmo/tests/wire.rs
crates/gsb-kit/src/aoi/tests*` boş).

| # | Commit | Kit değişikliği | Kanıtlayan test (önce kırıldı) | Demo tarafı |
|---|---|---|---|---|
| F1 | `c8ed9ba` | `integrate_borrowed`'ın çıkış döngüsü: kimlik artık bu shard'ın (`inner.wire_entity`) ve own kaydı ödünç kopyanın hücresindeyse (`book.cell_of_entity(e) == Some(c)`) `record_exit` atlanıyor; başka hücredeki ödünç kopya bayattır, çıkar. Modül belgesinin "defter o kimliği hiç görmedi" varsayımı düzeltildi | `sharded/tests/lent_arrival.rs` (4): hücresinde yalnız NPC'nin varışı (komşu gruba ne `removed` ne `cell_exits`; sonraki her full'da tek kez), oyuncunun varış one-shot full'unda kendini görmesi — ikisi düzeltmesiz kırıldı; farklı hücreye varış (bayat kopya kalmıyor; "her own kimliği atla" aşırı düzeltmesini kıran test) ve ters yön (komşuya giden entity'nin şeritten geri dönüşü, aynı tick ve bir tick sonra) — bugün doğru, sabitlendi | `findings::f1_…` çevrildi (`…_stays_visible_on_its_new_shard`: O, M'yi 258 tick boyunca her tick görüyor; A'nın saldırısı geçişten önce M'ye işliyor, sonra işlemiyor — geçişin kanıtı); `crossing`'in iki F1 geçici çözümü kalktı (aşağıda) |
| F2 | `fc4d10b` | `GridPartition2::with_diagonals()`: 8-komşuluk (kenarlar listede önce); yönlendirme (`first_hops`) ve şerit komşu grafını izliyor, köşegen alıcının `admits`'i köşe karesini tutuyor. Varsayılan 4-komşuluk | `sharded/tests/diagonals.rs` (4): komşu listeleri (2×2, 3×3 merkez/kenar/köşe, her ızgarada simetri), her ızgarada şah-hamlesi en kısa rotalar, köşe geçişinin tek adımda köşegene gitmesi, köşegen shard'ın köşeden ödünç alması — köşegenler yok sayılınca dördü de kırıldı | MMO `world::partition()` 8-komşuluğa geçti; `findings::f2_…` çevrildi (`…_lends_across_a_corner`: X köşedeki üç mob'u da görüyor); ışınlanma testi köşegene TEK adım / tek tick yolda |
| F3 | `deb1d4e` | `Planar` ve `GridPartition2` belgesine birim sözleşmesi; `Partition::debug_check_wire` (varsayılanlı, boş) — sharded odalar ihraç ettikleri her entity için çağırıyor, `GridPartition2` debug build'de wire izdüşümü konumunkinden bir şerit genişliğinden uzaksa panikliyor (release'de derlenip gidiyor). Derleme zamanı denetim mümkün değil: birim tiplerde yok. `GridPartition2` çocuk modüle taşındı (`space/partition/grid.rs`) | `space/tests/units.rs` (4): belirti (metre ↔ desimetre izdüşümünde çerçeve filtresi 128 m → 12,8 m), aynı birimli wire'ın haritanın her yerinde denetimden geçmesi, ince birimin panik (`should_panic`, yalnız debug) — denetim kapatılınca kırıldı —, sharded odanın tam olarak ihraç edilen entity'leri denetlemesi — çağrı eklenmeden kırıldı | MMO'nun kodek belgesi; MMO wire `Planar`'ı desimetreye çevrilince MMO entegrasyon testleri artık ilk ihraçta panikliyor |
| F4 | `cfc51dc` | `ParkPolicy { grace: Option<Duration>, to: ExpireTo }`; her odada `with_disconnect_policy(grace, to)`; `Game::may_release(world, entity)` (varsayılan `true`) ve her odanın `GameLogic::may_release`'i oyuna iletmesi. Varsayılan politika aynı | `common/park/tests.rs` (2): politika matrisi ve veto ALTI odada (open, AOI, team, PVS, sharded, sharded × spatial) vetolayan fikstür oyunuyla — sona sabit `AiHandover`, vetonun yok sayılması ve tek odanın iletmeyi unutması ayrı ayrı kırdı | MMO asıl çıkış sayacına geçti (`LOGOUT_GRACE`, sonra `Despawn`); `reconnect`: süre dolunca çıkış (aşağıda); çıkış botu `AiHandover` politikasıyla erişilebilir ve kendi gerçek-aktör testini koruyor. AI devri örneği: 2D demo'nun `park_policy::grace_expiry_hands_over_to_a_wandering_bot`'u (`detach_expired_ai == 1`, doğrulandı) |
| A1 | `c076fd0` | `TeamGame::spawn_team_player(world, conn) -> (Entity, Team)` (varsayılanı `spawn_player` + `team_of`); `TeamRoom::on_join` onu kit-içi `common::join_with` ile çağırıyor. Tek sözleşme farkı: `team_of` artık wire damgasından önce soruluyor (hiçbir kit oyunu orada kimliği okumuyordu) | `team/tests/spawn_team.rs` (2): iki üslü fikstür oyunu (katılım sırasıyla takım, `team_of` panikliyor) — oda `team_of`'u sorunca kırıldı; varsayılan yol conn paritesini koruyor | Arena `spawn_team_player`'ı eziyor (round-robin + üste spawn), `HomeBase` kalktı: bot üssü kit'in `TeamMember`'ından buluyor. Arenanın testlerinin iddiaları aynı (birim testleri `spawn_team_player` çağırıyor, bot testi takımı kit gibi kaydediyor); eski sıraya dönüş arenanın beş entegrasyon testini kırdı |
| A2 | `7356bd7` | İstemci kuralları (full/delta, uygulama sırası, baseline'sız delta, sıra boşluğu, yinelenen kare, keep-alive yakınsaması, one-shot full'un koşulsuz baseline sıfırlaması) `kit.proto`'ya taşındı, her oyun için yazıldı (hangi odalar delta gönderir) | yalnız yorum — alan, numara, tip değişmedi; bütün wire testleri değişmeden yeşil | demo'nun `game.proto`'su kısa özet + atıf; arena ve MMO aynaları atıf |
| A3 | `2d39767` | `Game::SNAPSHOT_OP` / `PRIVATE_OP` varsayılansız (oyun yazarı için kırıcı: sabitleri adlandırmayan `impl Game` derlenmez, E0046). Üç demo zaten açıkça bildiriyor (1003/1004, 1101/1102, 1201/1202) — bayt değişmedi; kit'in test oyunları fikstürün bloğunu (1901/1902) bildiriyor | `Game`'in iki doctest'i: sabitleri adlandıran asgari oyun derleniyor, aynısı onlarsız derlenmiyor (`compile_fail,E0046` — varsayılanlar varken kırıldı) | — |

**Bilerek değiştirilen test iddiaları** (başka hiçbir beklenen bayt,
kayıt sayısı ya da iddia değişmedi):
1. `gsb-demo-mmo/tests/findings.rs` — `f1_…`: "O, M'yi hiç görmüyor" →
   "O ve A, M'yi her tick görüyor" (+ saldırıyla geçiş kanıtı; A'nın
   girişi `-30` → `-20` m, saldırı menzili için); `f2_…`: "X köşegen
   mob'u görmüyor" → "X dört kaydın hepsini görüyor". Modül belgesi:
   bulgular Faz 5'te düzeltildi.
2. `gsb-demo-mmo/tests/crossing.rs` — oyuncu geçişi: "P kendini YALNIZ
   varış tick'inde kaybedebilir" izni kalktı (`P` her tick kendini
   görüyor); ışınlanma: iniş tick'inden sonra P'nin kendisinde ve D'de
   görünmesi ZORUNLU (eskiden görünüyorsa konumu denetleniyordu),
   "bir hücre batıya yürü, sonra görünür" adımı kalktı (P waystone'da
   duruyor ve görünüyor), yolda geçen süre `2` → `1` tick (F2: tek
   adım), test adı `…_non_adjacent_…` → `…_the_diagonal_shard_…`.
3. `gsb-demo-mmo/tests/reconnect.rs` — `grace_expiry_hands_the_character
   _to_the_logout_bot` → `grace_expiry_logs_the_character_out`: F4
   kilidi `(detach_expired_despawn, members) == (0, 2)` →
   `(detach_expired_despawn, detach_expired_ai) == (1, 0)`, `(members,
   detached) == (1, 0)`, karakter görünümden çıkıyor, resume kabul
   edilmiyor; eski AI-devri iddiaları yeni `an_ai_handover_policy_hands
   _the_character_to_the_logout_bot`'ta (açık `AiHandover` politikası).
4. `gsb-demo-arena/src/game/tests.rs` — iddialar aynı; iki test
   `spawn_player` + `team_of` yerine `spawn_team_player` çağırıyor, bot
   testi takımı kit'in `TeamMember`'ı olarak kaydediyor (`HomeBase`
   kalktı).

**Gözlemler (bu turda açılmayan, tetikleyicisiz):**
- Çekirdeğin `may_release` anlamı: veto yalnız SÜRESİZ bekletmede
  soruluyor (süreli bekletmede süre tavandır). "Çıkış sayacı + savaşta
  gecikme" birlikte bir çekirdek kararı ister; kit ikisini ayrı ayrı
  sunuyor. **→ Kapandı** (Faz 5 sonrası, `fix/may-release-deadline`):
  veto süreli bekletmenin deadline'ında da soruluyor, duran veto
  `RoomConfig::max_detach_hold` (varsayılan 10 dk, kopuştan itibaren)
  ile sınırlı; kit kodu değişmedi, MMO çıkış sayacı savaşta bekliyor
  (RECONNECT §17).
- `ShardedSpatialRoom` hâlâ `admits`'i uygulamıyor (F3'ün yan gözlemi):
  uzak ihraç kayıtları (ör. ışınlanan oyuncunun bölge dışı konumu) ara
  shard'ların defterine giriyor. Görünürlük hücreyle sınırlı olduğu için
  doğru; F1'in köşegen ışınlanma belirtisinin bir tetikleyicisi buydu
  (ara shard'ın şeridi hedef shard'a varıştan bir tick önce ödünç
  veriyordu), F1 düzeltmesiyle zararsız. **→ Kapandı** (Faz 5 sonrası,
  `fix/small-bundle`): "görünürlük hücreyle sınırlı, yani doğru"
  varsayımı genelde tutmuyor — hücre kenarı bölgeye göre büyükse bir
  grubun 3×3'ü komşunun UZAK kenarına uzanıyor (kit fikstüründe 4×4,
  bölge 25, kenar payı 6,25, hücre 20: shard 0'ın `Cell(-2,-2)`
  grubu, shard 1'in doğu kenarındaki x = -1 kaydını görüyordu; düz
  `ShardedRoom` aynı kaydı reddediyor). Kompozit artık şeridi deftere
  almadan önce aynı `admits` süzgecinden geçiriyor
  (`integrate_borrowed`); defter SÜZÜLMÜŞ görünümü tuttuğu için çerçeve
  dışına çıkan kayıt çıkış, geri giren giriş olarak okunuyor
  (`sharded/tests/frame_filter.rs`). Baytlar ve demo iddiaları aynı.
- 8-komşulukla 2×2'de her shard diğer üçünün şeridini alıyor: şerit
  trafiği kenar başına değil shard başına üç komşu (MMO'nun bilinçli
  seçimi; varsayılan 4-komşuluk).

**Kapanış kontrolü (Faz 5 kit'i üzerinde):** `cargo test -p gsb-demo
-p gsb-demo-arena -p gsb-demo-mmo` → **83 passed / 0 failed** (2D demo
43, arena 15, MMO 25 — MMO +1: `AiHandover` politikasıyla çıkış botu
testi). Üç görünürlük modeli, değiştirilmiş kit üzerinde; 2D demo'nun
bayt kilitleri dokunulmadan.

**Loadgen** (50 istemci; `32118b2` ↔ HEAD `2d39767` ikilileri —
sonraki commit yalnız doküman — dönüşümlü): Her koşuda `left=50`,
`errors=0`, `server_closes=0` (iki tarafta da):

| Koşu | step_p50_fine_us (taban / HEAD, 3 çift) | snap_total (taban / HEAD) | out_bps_per_conn (taban / HEAD) |
|---|---|---|---|
| tcp 3 sn | 56 40 32 / 56 40 24 | 4101 / 4100 | 11018 / 11063 |
| spatial 3 sn | 96 112 112 / 88 88 112 | 4135 / 4094 | 1944 / 1909 |
| sharded N=4, 8 sn | 40 48 40 / 40 40 32 | 11470 / 11445 | 7067 / 7027 |
| sharded × spatial N=4, 8 sn | 80 80 64 / 88 56 56 | 11539 / 11528 | 3322 / 3305 |

snap_total / out_bps ilk çiftin değerleri. Tek kovadan büyük fark
yalnız HEAD lehine (spatial ikinci çift −24 µs, sharded × spatial ikinci
çift −24 µs); HEAD'in tek yukarıdaki değeri bir kova (sharded × spatial
ilk çift +8 µs). F1 (şerit çıkışında bir tablo araması, yalnız şeritten
düşen kimlik başına) ve F2 (varsayılan 4-komşulukta değişmeyen yol) sıcak
yolda ölçülebilir iş eklemiyor; F3'ün denetimi release'de derlenip
gidiyor — gürültü içinde.

**Doğrulama:** 478 + 19 = **497** test / 0 hata / 1 ignored (F1 +4, F2
+4, F3 +4, F4 +3, A1 +2, A3 +2 doctest); `cargo clippy --workspace
--all-targets -- -D warnings` 0 uyarı; `cargo fmt --all --check` temiz.

### T sonucu — takım odasında delta (2026-09-25)

**Tamamlandı** (`kit/t-team-delta`, `6dcdf27..`; BACKLOG §1 satır 5).
Üç kod commit'i, her biri kendi başına yeşil: `c1ae42e` (one-shot
private full çerçeve yazıcısı ortak), `3e5054d` (ortak küme defteri +
`TeamRoom::with_delta`), `14cb51c` (arena delta modunda). `gsb-core`
değişmedi; `ClientView` ve hiçbir istemci değişmedi.

**Ne.** `TeamRoom::with_delta()` takım sisi odasının snapshot'larını AOI
odasının zarfı ve istemci kurallarıyla (`kit.proto`) gönderir: takım
başına, tick başına `removed` = takımın görünümünden çıkan wire id'ler,
`entities` = görünüme giren ya da WIRE değeri değişen kayıtların
upsert'leri (kodek nicemler: bir wire biriminden az hareket eden birim
yeniden gönderilmez); takım başına bir kez kodlanır, takımın üyeleri
paylaşır. FULL: taze takım grubuna (üyelerinin hepsini baseline'lar),
keep-alive temposunda (etkin ya da sessiz — yakınsama garantisi) ve
baseline'ı olmayan üyeye one-shot `Private.snapshot` olarak (oynayan
bir takıma katılım, resume, çalışma anında takım değişimi). Oturum yükü
(`Game::session_private`, arenanın `Welcome`'ı) değişmedi: oturumun ilk
private karesine biner — gerekirse one-shot full'un sonuna.

**Tasarım kararları ve elenenler:**

1. **Ortak motor, kopya yok.** Hücre motoru (`CellBook`/`CellPieces`)
   bir hücreyi bir kez kodlayıp görünümü tam hücrelerin birleşimi olan
   her gruba paylaştırır; grup-bağımsız hücre deltası bunun
   sağlamlığıdır. Görüş kümesi hücre birleşimi DEĞİL: bir düşmanın
   takımın kümesinde olması takımın birimlerine kesin mesafe testi, yani
   bir ızgara hücresi bir takımın görünümünde yarım olabilir. Bu yüzden
   `common` iki parça kazandı: **`SetLedger<W>`** (küme içerikli delta
   defteri — grup başına son gönderilen içerik, adım başına bir full
   kodlaması; `emit_full` bugünkü full yol, `emit_delta`, `full_frame`,
   `resync`) ve **`Baselines<K>`** (oturum başına "hangi grubun
   görünümüne baseline'lı" tablosu — AOI'nin `conn_view`'unun genel
   hâli). Üç delta odasının one-shot private full karesi artık tek
   yazıcı: `common::emit_private_full` (AOI ve sharded × spatial'daki iki
   elle yazılmış kopya kalktı — bayt birebir). *Elenen:* takıma özgü
   defter kopyası (A1'in tam tersi); `CellBook`'u görüş hücreleriyle
   zorlamak (yarım görünen hücre yanlış silme ya da sızıntı demek).
2. **Delta, SON GÖNDERİLEN içeriğe karşı.** Hücre motoru "önceki
   tick"e karşı çalışır (değişiklik listesi farkın kendisi); küme defteri
   grubun istemcilerinin tuttuğu şeye (son karenin içeriğine) karşı: kaç
   adım sessiz geçerse geçsin fark doğru. Maliyet: takım başına görünür
   küme üstünde bir geçiş — bugünkü full modun "değişmedi mi?"
   karşılaştırmasıyla aynı mertebe, ama `content.clone()` ve tam kodlama
   yok.
3. **`cell_exits` yok.** Hücre çıkışı istemcinin kayıttan
   hesaplayabildiği bir hücreyi bütünüyle unutturur — AOI'de doğru, çünkü
   görünüm tam hücrelerin birleşimi. Görüş kümesinde istemci üyeliği
   yeniden hesaplayamaz (almadığı konumlara ihtiyaç duyar) ve bir hücrenin
   düşmanları görünümden tek tek çıkar; görüş hücreleri yarıçap boyunda
   olduğu için bir hücre nadiren birkaç düşmandan fazlasını aynı anda
   bırakır — `removed` kimlik başına 2–3 bayt.
4. **Tazelik adım sayacıyla.** Çekirdek üyesi olan her grubu her adım
   bir kez sorar; bir önceki adımda sorulmamış grup üyesizdi → taze →
   full. Tick indeksi kullanılamaz: oda her k'ıncı global tick'te adım
   atabilir (`ctx.tick` global). Oda `update`'te kendi `step`'ini sayar.
   Yanlış "taze" yalnız fazladan bir full demek (güvenli).
5. **One-shot sırası (G3-2).** Oynayan bir takıma katılanın batch'inde
   önce grubun deltası (baseline yok → istemci düşürür, `gap_drops`),
   sonra one-shot full gelir — AOI'deki çekirdek sırası, GAME-MODULE
   G3-2'deki inceleme aynen geçerli; katılan başına bir düşürülen delta.
   Takımın bu adımki karesi zaten full'sa (taze / keep-alive) one-shot
   atlanır. Full kare adım başına bir kez kodlanır; taze kare,
   keep-alive ve bütün one-shot'lar aynı `Bytes`'ı paylaşır.
6. **Opt-in, varsayılan full.** (a) Sunucunun `communication` ekseni
   demo'nun `visibility = "team"`'ine `always-full` türetiyor ve `team ×
   delta`'yı "takımın delta paketlemesi yok" diye reddediyor (§ROADMAP
   "ortak DeltaSnapshotCodec"); varsayılanı çevirmek yapılandırmanın
   söylediğini yalan yapar ve demo'nun takım baytlarını değiştirirdi.
   (b) Küçük bir odada full kendi başına yeter ve istemcisi en basitidir.
   (c) Arena (ölçülen tetikleyici, G3-1) `arena_room`'da açıyor. Full mod
   bugünkü çıktının bayt bayt aynısı: tohumlu rastgele bir koşuda eski
   kodlayıcının kopyasına karşı her kare ve bir kare literal baytlarla
   kilitli (`team/tests/delta/full_only.rs`). *Elenen:* delta'yı
   varsayılan yapmak (yukarıdaki a); ayrı bir `SnapshotMode` enum'u
   (tek bayrak yetiyor; `all`/`pvs` aynı `with_delta` ile benimser).
7. **Sınırlı durum:** takım başına son gönderilen içerik (görünüm,
   her kaydın wire değeri kendi parmak izi) + bu adımın full'u; oyuncu
   başına baseline'lı takım (leave ve resume'da silinir). Geçmişle
   büyüyen hiçbir şey yok.

**Testler** (önce kırılan: `with_delta` işlemsizken — turdan önceki
oda — yeni takım testlerinin 6'sı kırıldı; arena'da `arena_room` full
modda kalınca 3'ü): `common/ledger/tests.rs` (5: fark = tam olarak
`removed` + upsert, tel sırası; sessizlikte bayt yok; adım boşluğu →
taze; full adım başına bir kez; full mod = eski kare; baseline kuralı),
`team/tests/delta*.rs` (10: görüşe giren upsert / çıkan `removed`,
yalnız wire değeri değişince upsert, keep-alive full, taze grup,
yeniden doğan grup, oynayan takıma katılana one-shot, takım değişimi +
resume one-shot, leave baseline'ı siler; **900 tick'lik tohumlu
rastgele koşu**: üç takım, katılma/ayrılma/yürüme/ışınlanma/takım
değiştirme/nötr doğma-ölme — kayıpsız delta istemcisi `ClientView`
üzerinden HER tick takımın gerçek içeriğini, kayıplı istemci her
keep-alive tick'inde full-only kâhinin görünümünü tutuyor; full modun
bayt kilidi), oturum yükü testine "team (delta)" satırı. Arena:
`tests/delta.rs` (gerçek oda aktörü üzerinden delta ve full ikiz
odalar, her denetim noktasında her istemcinin görünümü aynı; gerçek
bir delta karesi iki tanımdan aynı içeriğe çözülüyor ve kit'in
düzenini taşıyor), `wire.rs` (full modun gerçek kareleri değişmeden
kilitli; dördüncü katılanın ilk private karesi one-shot full + `Welcome`).
Loadgen botu zaten `ClientView` üzerinde; birim testi delta uygular.

**Mutation-check** (her biri yedekten geri yüklenerek; hepsi en az bir
testi kırdı): `removed` yazılmıyor; değişen değer yeniden
gönderilmiyor; her kayıt her tick gönderiliyor; hiç taze grup yok;
delta modda keep-alive full yok; one-shot hiç yok; grubun full'u
varken de one-shot; baseline grubu yok sayıyor; resume baseline'ı
silmiyor; leave silmiyor; full mod hiç sessiz değil; varsayılan oda
delta yolunu koşuyor; full kare adımlar arası önbellekte; adım sayacı
yok; full modun başlığında delta bayrağı. Arena'da: `removed` yok,
one-shot yok → `tests/delta.rs` (+ `wire.rs`) kırıldı.

**Ölçüm** (release, 32 çekirdek, `--duration 10 --write-stall-secs 0`,
`6dcdf27` ↔ HEAD dönüşümlü çiftler; makine iki başka ajanın
build'leriyle yüklüydü — 1 dk yük ortalaması tabloda). Her koşuda
`left = N`, `errors=0`, `server_closes=0`, `server_hz` 29,98–30,02:

| Senaryo (çift) | Yük | `out_bps_per_conn` taban / HEAD | peak payload (B) | `snap_overflows` | step p50/p90 fine (µs) taban / HEAD | records/tick | HEAD `deltas` / `fulls` / `private_fulls` |
|---|---|---|---|---|---|---|---|
| arena 200 (3) | 19–39 | 43 628–44 559 / 39 166–39 938 (−%10) | 1885–1937 / 1900–1967 | 677–699 / 522–526 | 552/856, 888/4096, 664/1232 / 864/2640, 640/1312, 632/1080 | 431–433 / 381–382 | 54,9–55,6 k / 1934–1953 / 51–67 |
| arena 500 (3) | 26–33 | 108 320–109 985 / 96 424–99 122 (−%10) | 4906–5211 / 5073–5326 | 793–798 / 807–821 | 1360/2112, 1512/2608, 2568/4096 / 1512/2288, 2480/4096, 2448/4096 | 1086–1098 / 985–1004 | 130–134 k / 4570–4738 / 321–341 |
| arena 1000 orkestre, 2 süreç (4: 2 AB + 2 BA) | 30–46 | 229 281–231 932 / 207 246–213 521 (−%9,5) | 10 152–10 426 / 10 090–10 443 | 905 / 907–924 | 2304/2960, 2440/3352, 2328/2944, 2672/3720 / 3512/4096, 2696/4096, 2664/3192, 2416/3128 | 2164–2198 / 1956–1984 | 275–280 k / 9725–10 816 / 872–997 |
| arena 500 rUDP `--stagger-ms 5` (2) | 28–31 | 102 481–102 533 / 93 691–93 729 (−%8,6) | 5126–5138 / 5064–5078 | 753–754 / 774 | 1336/2232, 1384/2480 / 1280/2056, 1360/2040 | 1092 / 1003 | 126 k / 4587 / 480 |

rUDP'de `frag_reassembled` 123 193–123 363 → 122 377–122 483 (−%0,7),
`frag_dropped` 0 / 0, `retrans_out` 161–292 / 290–298. HEAD'de
`gap_drops` = `private_fulls` (her geç katılan bir delta düşürüyor,
G3-2); orkestre 1000'de fan-out `dropped` 1542–1815 → 1035–1332,
`server_cpu_s` 3,8–4,3 / 3,8–4,9, `clients_cpu_s` 12,2–13,7 / 14,6–16,2
(dört çiftin dördünde HEAD yüksek — istemci tarafında delta birleştirme;
`ClientView` bu turun kapsamı dışında, kayıt).

**Bulgu — kayıt başına delta arenada ~%10 kazandırıyor, parçalanmayı
düşürmüyor.** Arena botunun her birimi hareket eden bir hedefi 12 m/s'le
kovalıyor (GAME-MODULE G3 "Botlar"); wire santimetre, yani görünür
birimlerin ~%85–90'ı (records/tick oranından, keep-alive full'ları düşülünce) HER tick yeni bir wire değeri
taşıyor. Delta yalnız duran birimleri ve görünümü değişmeyen takımları
atlıyor; bir takımın karesi 500'de hâlâ ~4,5 KB, 1000'de ~9 KB — MTU'nun
üstünde, parçalanma aynı. Mekanizma doğru ve yerinde (duran birimlerin
yarısı olan tohumlu kit koşusunda delta baytları full'un %38'i: 169 109
/ 447 873 B); arenada daha fazlası kayıt değil DEĞER düzeyinde iş ister:
(a) kodek seam'inde son gönderilen değere göre göreli kayıt (40 cm'lik
adım 2 yerine 1 baytlık zig-zag; kit + istemci çözücüsü değişir), (b)
varlık başına yayın hızı (BACKLOG A10), (c) takım karesini parçalara
bölmek (grup başına birden çok kare — çekirdek). Hiçbiri bu turda
yapılmadı.

**Yan bulgu (full mod, önceden vardı):** çalışma anında, zaten birimini
gören bir takıma geçen oyuncu için o takımın içeriği değişmez → kare
yok → oyuncu eski takımının görünümünü bir sonraki değişikliğe ya da
keep-alive yeniden gönderimine kadar tutar. Delta modunda bu oyuncu
one-shot full alıyor (rastgele koşunun full-only kâhini bu yüzden yalnız
keep-alive tick'lerinde karşılaştırılıyor).

**A1 — ortak delta motorunun `all` / `pvs` adopsiyonu.** Artık neredeyse
mekanik: iki oda da bugün tam olarak takım odasının kalıbını
kullanıyor (grup başına içerik haritası + grup başına son içerik +
`write_full_header`/`put_entity_records`). Gereken: (1) `last`'ı
`SetLedger` ile değiştirmek (`emit_full` bayt birebir), (2) bir `delta`
bayrağı ve `with_delta`, (3) `keepalive` (`resync`) ve `private`'ta
`Baselines` + `emit_private_full` (~30 satır yapıştırıcı — oturum yüzeyi
kural gereği oda başına), (4) oda kendi adım sayacı. Oda başına fark:
`OpenRoom` içeriği `snapshot`'ta topluyor (tek grup, private full için
ayrıca saklanmalı); `SectorRoom` sektör birleşimini `snapshot`'ta
kuruyor — private full için içeriği adım başına önbelleğe almalı ya da
yeniden kurmalı. Sunucu tarafı: `communication = "delta"`'yı `all`/
`team`/`pvs` için kabul etmek (`SingleDelta` reddi) — sunucu işi. Kazanç
tetikleyicisi aynı: çoğu birimi duran bir iş yükü; bu turun ölçümü
hareketli bir iş yükünde kazancın küçük olduğunu gösteriyor.

**Doğrulama:** 704 → **722** test / 0 hata / 1 ignored (+18: defter 5,
takım 10, arena 2, loadgen botu 1); kapanış kontrolü `cargo test -p
gsb-demo -p gsb-demo-arena -p gsb-demo-mmo` → **98 passed / 0 failed**
(arena 15 → 17); `cargo clippy --workspace --all-targets -- -D
warnings` 0 uyarı; `RUSTDOCFLAGS="-D warnings" cargo doc --workspace
--no-deps` temiz; özellik derlemeleri (`--no-default-features`, tek tek
`game-demo` / `game-arena` / `game-mmo`) temiz.

### W1 sonucu — team × sharded kompoziti (2026-09-25)

**Tamamlandı** (`kit/w1-team-sharded`, `e99d90c..`; BACKLOG §1 satır
6'nın ilk yarısı — W2 doğrulama oyununu bunun üstüne kurar). Tasarım,
§8'den sapmalar, çekirdek seam'i, testler, mutasyonlar ve ölçüm:
CROSS-SHARD §8b. Burada kit yüzeyi.

**Yeni kompozit: `ShardedTeamRoom<G, P, V>`** (`sharded/team.rs`;
`G: ShardGame + TeamGame`, `P: Partition<Wire<G>>`, `V: Vision`) —
`ShardedSpatialRoom` kalıbı: grid protokolü (göç, border şeridi, seam
kancaları, park, RPC) sarılan `ShardedRoom<G, P>`'de; kompozit takım
yüzeyini ekler. `GroupKey = Team`, `Strip = Wire<G>` (C1'in seam
kancaları bu tipe bağlı — değişmedi), göç durumu `TeamMig<G::Mig> {
kit: KitMig<G::Mig>, team: Option<Team> }` (`TeamMember` kit'in
bileşeni, oyunun `capture`'ı onu bilmez; varışta yeniden yazılır).

```rust
ShardedTeamRoom::with_shard(inner: ShardedRoom<G, P>, vision: V,
                            lent_pos: fn(&Wire<G>) -> Option<V::Pos>)
    .with_delta()                  // takım odasının delta modu
    .with_team_budget(n)           // varsayılan DEFAULT_TEAM_BUDGET = 1024
    .with_crystallize(..) / .with_disconnect_grace(..) / .with_disconnect_policy(..)
room.over_budget()                 // bütçenin kestiği kayıt, kümülatif
```

- **İçerik, çekirdeğin TEAMS fazında** (`ShardLogic::team_exchange`,
  faz 5b): takım başına önce üyeler (oyuncular ve `TeamMember`'lı NPC'ler
  — gözcü kuleleri), sonra bu shard'ın KENDİ birimlerinin gördüğü, üyesi
  olmayan her şey (kendi entity ve border şeridinin ödünç kayıtları;
  ödünç kayıt görüşe `lent_pos` ile girer, takımı bilinmez → yalnız
  HEDEF). Bu küme export edilir (bütçeyle kesilir); görüntülenen takımın
  içeriğine ayrıca bu shard'ın nötrleri ve diğer shard'ların O TAKIM
  için export ettikleri eklenir. Bir wire bir kez; yük önceliği
  **yerel > ödünç > ithal**, görünürlük birleşim. İthal asla yeniden
  export edilmez. Snapshot çağrıları bu tick'in içeriğinden okur
  (TeamRoom'un `update` önbelleğinin karşılığı).
- **Kodlama.** İçerik değeri `Shown<W> { Typed(W), Encoded(Bytes) }`;
  `common::SetLedger`'ın yazım sınırı `RecordCodec`'ten crate-özel
  `WriteRecord<W>`'ye gevşedi (`RecordCodec` için blanket impl —
  mevcut odalar bayt bayt aynı). Kompozitin yazıcısı ithal gövdeyi aynı
  `entities` zarfına olduğu gibi koyar (`common::put_entity_body`):
  istemci ayırt edemez. Export gövdeleri wire başına önbellekli (değer
  değişmedikçe aynı `Bytes`).
- **Oturum.** Göçle gelen (ya da gidip dönen) oyuncunun baseline'ı
  düşer: delta modunda one-shot private full (spatial kompozitin
  taze-üye kuralı). Katılım `TeamGame::spawn_team_player_as` üstünden.

**`TeamGame` yüzeyi (K4 kalıntısı kapandı).** `spawn_team_player_as(&mut
self, world, conn, identity) -> (Entity, Team)`, varsayılanı
`spawn_team_player` — arena'nın üs doğumu değişmez. `TeamRoom` artık
`on_join_as`'ı uygular (`on_join` boş kimlikle iner) ve bunu çağırır.
`Team` artık `PartialOrd + Ord` (takımlar sıralı küme anahtarı).
`ShardGame` değişmedi.

**Paylaşılan parçalar.** `ShardedRoom::admit(world, spawn)` (katılımın
iki kimliği + tablolar; `sharded/room/join.rs`) sharded oda ve
kompozitin ortak katılımı. `testing::fix_lent_pos` ve kompozitin fixture
kurucusu.

**Kit'e dev-dependency: `tokio` (`test-util`).** Gerçek registry + dört
shard aktörü testleri (`sharded/tests/team_actors*`) duraklatılmış
saatte koşar: ticker yalnız her aktör boştayken ilerler, yani bir
tick'in export'ları sonraki tick başlamadan rölelenir ve metrik
kanalındaki adım bariyeri kesindir. Kit hâlâ hiçbir oyuna bağlı değil
(`manifest.rs` kilidi yeşil).

**Doğrulama:** 740 → **776** test / 0 hata / 1 ignored; kapanış
kontrolü, özellik derlemeleri, clippy, rustdoc kapısı ve dönüşümlü
loadgen A/B'si CROSS-SHARD §8b.7'de.

### W2 sonucu — dördüncü doğrulama oyunu "Cephe" (2026-09-25)

**Tamamlandı** (`demo/w2-war`, `3e74f67..`; BACKLOG §1 satır 6b).
Workspace'te yeni `crates/gsb-demo-war`: üç fraksiyonlu bir savaş,
görünürlük modeli **shard'lı bir harita üzerinde takım sisi** — W1'in
`team × sharded` kompozitinin ilk kullanıcısı
(`ShardedTeamRoom<WarGame, GridPartition2<Pos3>, VisionGrid2<Pos3>>`,
delta modu). Oyuncu kendi fraksiyonunun her birimini harita genelinde,
bir düşmanı yalnız fraksiyonundan bir birim (oyuncu ya da gözcü
kulesi) onu görürken görür — hangi shard'da olursa olsun. **Kabul testi
olarak koştu: kit'e tek satır dokunulmadı** (`git diff 3e74f67.. --stat
-- crates/gsb-kit crates/gsb-demo crates/gsb-demo-arena
crates/gsb-demo-mmo crates/gsb-protocol crates/gsb-net` boş). Çekirdeğe
yalnız ölçüm için dokunuldu (takım sayaçlarının metrik yoluna terfisi,
BACKLOG A26 — oyunun çalışması için değil; CROSS-SHARD §8b.8). Savaş
`gsb-kit` + `gsb-core` + `gsb-protocol`'e bağlı, hiçbir demoya değil;
yalnız kit'in public yüzeyini görüyor — kanıt yapısal (§11.1).

**Cephe'nin kendisi (oyunun işi, §2):**

| Parça | Seçim | Gerekçe |
|---|---|---|
| Konum | `Pos3 { x, y, z: f32 }`, metre, **y yukarı**; `Planar` → `[x, z]` | shard'lama ve görüş zemin düzleminde (MMO gibi); kule platformu (y = 12 m) yalnız wire'da |
| Harita | 1 600 m × 1 600 m (`WORLD_HALF = 800`), 2×2 shard (`GridPartition2::new(4, 800).with_diagonals()`, şerit 200 m), üç üs üç bölgede, dördüncü bölge üssüz (çekişmeli) | 8 000 dm: her zemin koordinatı 2 baytlık zig-zag varint; köşegen komşuluk: orta nokta dört bölgenin köşesine 85 m |
| Birimler | oyuncular; **her fraksiyonun her bölgede bir gözcü kulesi** (12 kule, `TeamMember`'lı statik NPC — kit onu yetim olarak damgalar, görüş kaynağı); iki ele geçirme noktası (üssüz bölgede: ortada ve bölge merkezinde) | fraksiyonun oyuncusu olmayan shard'da da gözü olsun — kompozitin var olma sebebi olan senaryo (CROSS-SHARD §8b: shard 2'deki kule, shard 3'teki oyuncuya düşman gösterir) |
| Görüş | `VisionGrid2<Pos3>`, **tek yarıçap 60 m** (kuleler dahil) | birim başına yarıçap kit'te yok (A8); gerekmedi — aşağıda bulgu W2-2 |
| Codec | `Wire = WarWire { x, y, z: i32 dm, kind, faction, hp }`; gövde `UnitRecord { entity = 1; x, y, z = 2..4; Kind kind = 5; uint32 faction = 6; uint32 hp = 7 }`; `Dirty = Or<(Changed<Pos3>, Changed<Unit>)>` | desimetre MMO'nun gerekçesiyle; **fraksiyon kayıtta** (1 tabanlı, 0 = yok): takım karesi kimin müttefik olduğunu söylemez, kayıt söyler — istemci dost/düşmanı ayırır |
| Fraksiyon ve yerleşim (K4) | kayıtlı karakter (fraksiyon + konum) doğrulanmış kimlikle (`Realm::logins`); kaydı olmayan: **kimliğin FNV-1a özeti mod 3**, o fraksiyonun üssünde (özetle ±10 m) — `TeamGame::spawn_team_player_as` ikisini tek adımda seçer; sunucunun yönlendiricisi aynı `Realm::placement(identity)`'yi okur | deterministik: yönlendirici (yalnız kimliği görür) ve spawn paylaşılan durum olmadan anlaşır; dönen kaydısız oyuncu aynı tarafa düşer. Elenen: katılım sırası (arena) — yönlendirici odanın katılım sayısını göremez; oturum kimliği — sunucu-geneli, yeniden bağlanan taraf değiştirir |
| Oturum yükü | `Welcome { faction, factions }` (`Private.game`, `Game::session_private`) | arena'nın kalıbı, kompozitte değişmeden çalıştı: geç katılanın one-shot full'uyla birlikte gelir |
| Oynanış | `MoveTo` (7 m/s, zeminde), `Attack` (20 m, 25 hasar, 4 darbede düşer); düşen oyuncu üssünde tam canla (bir ışınlanma: gerekirse göç); kuleler ve noktalar vurulamaz | küçük tutuldu — doğrulama oyunu |
| Seam ötesi saldırı | MMO'nun kalıbı: yerel hedef `Seam::local` ile, ödünç hedef `Seam::lent` + `emit` (saldıranın shard'ı menzil ve tarafı ödünç kayıtta denetler); sahip `apply_remote_effect`'te yeniden denetler (yaş ≤ 3 tick, taraf, menzil + 2 m), hasarı tavanlar; öldürme **sahibin** shard'ının akışında bir kez (`Hit`) | kit değişmeden yeniden kullanıldı; crystallization açılmadı (isteğe bağlı; `with_crystallize` kompozitte hazır) |
| Ele geçirme | noktada `CAPTURE_RADIUS` (15 m) içinde 90 tick boyunca YALNIZ bir fraksiyonun ayakta oyuncuları → nokta o fraksiyonun olur: `Unit::faction` + kit'in `TeamMember`'ı (çalışma anında yazılan takım) | iki nokta da dikişlerden ≥ 60 m içeride: ele geçirme tek shard'da karar verilir |
| Ayrılma | kit'in park defteri: süre (varsayılan 30 sn) dolunca geri çekilme botu birimi üssüne yürütür (arena'nın seçimi) | savaşta çıkış sayacı yok |
| Oda | `WarShard` takma adı; kurucu `war_shard(index, &realm)` (serbest fonksiyon — E0116) | — |
| Wire | `proto/war.proto` (`gsb.war`): `MoveTo`, `Attack`, `Kind`, `UnitRecord`, `Welcome`, kit zarfının tipli aynaları `WorldSnapshot` (`cell_exits = 4` aynalanmadı) ve `Private` (`game = 4` → `Welcome`); `gsb.kit.InputAck` olduğu gibi | — |
| Opcode'lar | `WAR_MOVE_TO = 1300`, `WAR_SNAPSHOT = 1301`, `WAR_PRIVATE = 1302`, `WAR_ATTACK = 1303` | demo (1000–), arena (1100–), MMO (1200–) bloklarından ayrık |

**Kit'ten kullanılanlar** (tamamı public): `codec::RecordCodec`;
`game::{Game, TeamGame, ShardGame, InputSeq}` (`spawn_team_player_as`,
`session_private`, `ingest_seam`, `apply_remote_effect`);
`sharded::{ShardedRoom, ShardedTeamRoom, TeamMig, Seam}`
(`with_shard(inner, vision, lent_pos)`, `with_delta`, `game_mut`);
`space::{Planar, GridPartition2, VisionGrid2, shard_at, Partition}`
(`Partition` yalnız birim testinde); `team::{Team, TeamMember}` (oyun
kulelere ve ele geçirilen noktaya kendisi yazar); `identity::WireId`;
`proto::InputAck`; testlerde `client::{ClientView, ClientDecoder}`.
Kullanılmayan her şey (diğer odalar, `Grid2`/`CellSpace`, `Spatial`,
`VisionGrid3`, `Crystallize`) gerekmedi.

**Testler** (26: 18 birim + 8 entegrasyon; entegrasyonların yedisi
**canlı bir registry (takım hub'ı) + dört gerçek shard aktörü**
üzerinden — kit'in `team_actors` düzeneği crate-içi olduğu için public
API'den eşdeğeri kuruldu (`tests/common`: `Registry::new` + `Ticker`,
metrik kanalı adım bariyeri, istemciler kit'in `ClientView`'ıyla):

| Test | Kilitlediği | Mutation-check (savaş tarafında, yedekten geri yüklenerek) |
|---|---|---|
| `fog::allies_are_seen_map_wide_and_a_faction_sees_nothing_it_has_no_eyes_on` | shard 0 ve 3'teki iki müttefik birbirini, her fraksiyon kendi dört kulesini görüyor; üçüncü fraksiyonun görünümü koşu boyunca yalnız kendi kayıtları; shard 3'teki oyuncu shard 3'ün iki nötr noktasını görüyor, shard 0'daki görmüyor | kule yok → kırıldı; kulede `TeamMember` yok → kırıldı; wire `Planar` desimetre → kırıldı; kayıtta fraksiyon yok → kırıldı |
| `fog::an_enemy_is_seen_through_a_far_tower_on_another_shard` | shard 2'de fraksiyon 0'ın kulesine yürüyen düşmanı 850 m ötede shard 3'teki fraksiyon 0 oyuncusu, kule gördüğü ilk kayıttan (≤ 60 m) son kayda kadar görüyor; öncesinde ve sonrasında görmüyor; üçüncü fraksiyon hiç görmüyor. **Gerçek saatte** (bulgu W2-4) | kule yok / `TeamMember` yok / yarıçap 30 → kırıldı |
| `neutral::an_unclaimed_point_follows_the_shard_a_captured_one_the_faction` | W1 nötr kuralı Cephe'de: sahipsiz nokta shard 3'teki herkese (640 m ötedekine de), başka shard'da yalnız görüşle (noktadaki müttefiği aracılığıyla); ele geçirilince fraksiyonun birimi — alan oyuncu ayrıldıktan sonra da fraksiyonun uzaktaki oyuncusu görüyor, rakip fraksiyon artık görmüyor | ele geçirmede `TeamMember` yazılmıyor → kırıldı |
| `placement::logins_appear_where_the_realm_places_them_and_learn_their_faction` | kayıtlı karakter kaydında, kaydının shard'ında; kaydısız üssünde, özet fraksiyonunda; her oturuma bir `Welcome` (geç katılana one-shot full'uyla) | kayıt yok sayılıyor → kırıldı; `Welcome` yok / 0 tabanlı → kırıldı; full mod (delta yok) → kırıldı |
| `combat::a_kill_across_a_seam_is_credited_once_by_the_victims_shard` | x = 0 dikişinin iki yanında 10 m: dört darbe sahibin shard'ında (1) uygulanıyor, öldürme BİR kez, saldıranın wire id'siyle; düşen üssünde tam canla; artık vurulacak bir şey yok | yeniden doğma yok → kırıldı; kaynak shard da öldürmeyi sayıyor → kırıldı (çift kredi); `lent_pos = None` → kırıldı (seam ötesini göremiyor); uzak etki yok sayılıyor → kırıldı |
| `combat::no_friendly_fire_no_reach_and_a_local_blow_lands_locally` | dikişin iki yanında müttefike ve menzil dışındaki düşmana darbe yok; menzildeki yerel düşmana darbe yerel shard'da | yerel dost ateşi / yerel menzil yok → kırıldı; seam ötesi taraf ya da menzil denetimi TEK BAŞINA kaldırılınca → **hayatta kaldı** (sahibin yeniden denetimi yakalıyor), sahibin denetimi tek başına → hayatta kaldı (saldıranınki yakalıyor); ikisi birlikte → kırıldı: iki katman birbirinin yedeği |
| `wire::kit_envelope_and_war_mirror_encode_identically` | kurulmuş kareler (full, delta, one-shot full + `Welcome` + RPC yanıtı, ack) iki tanımda aynı bayt | — |
| `wire::real_shard_frames_decode_identically_through_both_definitions` | gerçek shard kareleri — **başka shard'ın kodladığı ithal gövdeler dahil** — iki tanımdan aynı içeriğe, full'lar kit'in kendi baytlarına; `removed`'lı delta, one-shot full, dört `Welcome`, ack | `Welcome` yok → kırıldı; full mod → kırıldı |

Birim testleri: nicemleme, 1 tabanlı fraksiyon, kayıt gövdesi = tipli
`UnitRecord` (köşe kaydı 17 bayt), wire `Planar`'ı metre (mutasyon:
desimetre → kırıldı), 8-komşuluk (mutasyon: 4-komşuluk → kırıldı),
harita geometrisi (kuleler şerit dışında, üslerden > 110 m, noktaları
görmüyor, birbirini görmüyor), yerleşim ve özet kuralı (mutasyon: sabit
fraksiyon → kırıldı), `Welcome`, her bölgede üç kule + noktalar,
ele geçirme (mutasyon: çekişme sayacı sıfırlamıyor → kırıldı; ilk
hâliyle HAYATTA KALMIŞTI — test "çekişmede alınmadı" iddiasıyla
sıkılaştırıldı), koşu, göç durumu, geri çekilme botu, opcode bloğu,
uzak etki yükü.

**Tasarım bulguları.** Kit'in public API'si Cephe'yi **engellemedi** —
kit değişikliği yok, kit iç öğesi kopyalanmadı. Kayda geçenler:

1. **W2-1 — nötr kuralı shard bölgesine bağlı (A27, kanıtlı).** W1'in
   kuralı (nötr kendi shard'ında herkese, başka yerde sisle) oyuncuya
   görünen bir bölüm artefaktı üretir: shard 3'ün 640 m uzağındaki
   oyuncusu sahipsiz noktayı hiçbir göz olmadan görür, noktaya 100 m'deki
   shard 0 oyuncusu görmez (`neutral` testi bunu sabitliyor). Tek oda
   `TeamRoom`'da "herkese" doğruydu; haritayı bölünce "aynı bölgede
   duranlara" oldu. Cephe kuralı kabul etti (sahipsiz nokta bir hedef,
   harita durumu değil) ve ele geçirilen noktayı fraksiyonun birimi
   yaptı — bu yolla sahibi onu harita genelinde görür. **Harita geneli
   nötr** (her takımın görünümünde, her yerde) kit değişikliği ister:
   en küçük hâli entity başına bir "harita geneli" işareti (kompozit onu
   her görüntülenen takımın export'una üye gibi koyar). Kit'siz tek hile
   ters yönde: hiçbir oyuncunun görmediği hayalet bir takımın üyesi
   yapmak nötrü her yerde tutarlı sisle gösterir (export'a takım başına
   birkaç kayıt, röle yok) — kullanılmadı.
2. **W2-2 — birim başına görüş yarıçapı gerekmedi (A8 açık kalır).**
   "Kule daha uzağı görür" yerine "her fraksiyonun her bölgede bir
   kulesi" tek yarıçapla aynı oyun ihtiyacını karşıladı. Kit'siz yol
   bilinen hile: yarıçapı konum tipine gömen kendi `Vision`'ı — ama
   ızgara sözleşmesi hücreyi EN BÜYÜK yarıçapa göre boyutlandırmayı
   ister, yani her oyuncunun çift testi kule yarıçapının hücresine
   çıkar. Tetikleyici değişmedi.
3. **W2-3 — takım bütçesi üyeleri wire sırasıyla keser.** "Önce üyeler"
   kuralının içinde sıra wire id'si: ilk tick'te doğan kuleler her
   zaman oyunculardan önce gelir, bütçe kestiğinde en yeni katılanlar
   gider (`war_e2e::the_war_table_budget_reaches_the_shards` bunu
   bütçe 1 ile gösteriyor). Varsayılan bütçede (1 024/takım/tick) 1 000
   oyuncuda bile kesme yok (takım başına ~260 kayıt); kesme olursa da
   kurbanı keyfi. Ayrıca kit'in `over_budget` sayacı barındırıcıya
   ulaşmıyor (oda `Box<dyn ShardLogic>`; F9'un "kit sayaç seam'i"
   ihtiyacının ikinci ailesi). Kayıt; kod değişmedi.
4. **W2-4 — duraklatılmış saatte yürüyüş durur (çekirdek, test
   ergonomisi).** `Ticker` tick'leri `std::time::Instant` ile damgalar,
   shard'ın `dt`'si iki damga arası: tokio'nun duraklatılmış saatinde
   tick'ler art arda gelir, `dt` mikrosaniye, 7 m/sn'lik koşu yerinde
   sayar. Kit'in `team_actors` testleri ışınlanmayla dolaştı; Cephe'nin
   yürüyen senaryosu gerçek saatte koşuyor (bariyer yine kesin, röle
   bir tick gecikebilir). En küçük değişiklik: ticker'ın damgasını
   `tokio::time::Instant`'tan almak — üretimde aynı, testte sanal. Kod
   değişmedi.

**Gözlemler (bulgu değil):** kompozit, çalışma anında `TeamMember`'ı
değişen bir NPC'yi (ele geçirilen nokta) hiçbir şey yapmadan doğru
gösteriyor (içerik her tick yeniden kuruluyor); `session_private`
kompozitin one-shot full'una biniyor; ithal gövdeler istemcide yerel
kayıtlardan ayırt edilemiyor (wire testi); `Game::SNAPSHOT_OP` /
`PRIVATE_OP` artık varsayılansız (Faz 5) — ezmek zorunluydu.

**Yük altında** (release, `--duration 10 --write-stall-secs 0`; ayrıntı
GAME-MODULE "W2 sonucu", röle bulguları CROSS-SHARD §8b.8): 1 000
oyuncuda (orkestre) `server_hz` 30, adım p50/p90 1,4–1,7/2,4–3,1 ms,
`errors=0`; istemci başına görünüm ~956 birim, `out_bps_per_conn`
~365 KB/sn — arena 1000'in (delta) ~1,75 katı. Bant röleden değil
senaryodan: "müttefik harita geneli" her istemciye O(N) kayıt/tick
demek (hareketli birimlerin çoğu her tick yeni desimetre değeri).
Röle ucuz ve kayıpsız: shard başına tick başına bir export, export
başına ~0,78·N kayıt, yayılım tam 3, düşme 0.

**Doğrulama:** 776 → **818** test / 0 hata / 1 ignored; kapanış
kontrolü `cargo test -p gsb-demo -p gsb-demo-arena -p gsb-demo-mmo -p
gsb-demo-war` → **124 passed / 0 failed** (2D demo, arena, MMO 98 +
savaş 26); dört oyun tek kit üzerinde, dört görünürlük modeli.

### A22 — değer düzeyinde delta: ölçüm ve seçenekler (2026-09-25)

**Faz 0: ölçüm + tasarım, wire değişikliği YOK** (`kit/a22-value-delta`;
BACKLOG A22). Soru: kaydı grubun son gönderdiği değere göre göreli
kodlamak (A22) bandı ne kadar düşürür, istemci sözleşmesine (`kit.proto`:
upsert'ler mutlak ve idempotent, baseline'lı delta boşluk olsa da
uygulanır) ve encode-once'a ne mal olur — ve aynı baytı daha ucuza
kazandıran başka bir kaldıraç var mı? Kod tarafında yalnız bir ölçüm
aracı eklendi; kit, çekirdek, oyunlar ve hiçbir istemci baytı değişmedi.

**Araç (repoda, kalıcı): `gsb-loadgen --capture DIR [--capture-clients
K]`.** Örneklenen K istemcinin (varsayılan 8, id'lere eşit aralıkla
yayılmış — farklı takımlar/hücreler) aldığı oyun bandı karelerini
(grup snapshot'ı, private kare) ve JOIN sonucunu (istemcinin kendi wire
id'si) geliş sırasıyla bellekte tutar, oturum bitince istemci başına bir
`client-<id>.gsbcap` dosyası yazar (biçim `loadgen/capture.rs` başında).
İstemcinin gönderdiğini ve uyguladığını değiştirmez; yalnız düz koşuda
(orkestre/`--serve`/churn reddedilir). Kilit: `loadgen_games.rs`
`loadgen_captures_the_frames_its_clients_applied` — dosya kit'in
`ClientView`'ından geçirilince istemcinin `CLIENT` satırındaki
snapshot/full/delta/private full/`gap_drops`/ack/görünüm sayılarını
birebir veriyor (mutasyon: private kareleri kaydetmemek → kırıldı; küçük
snapshot'ları atlamak → kırıldı). Neden kalıcı: A22'nin uygulama turu,
A10 ve A14 (zstd) aynı "önce/sonra" baytına ihtiyaç duyacak.

**Çözümleyici (repoda DEĞİL, bu turun scratchpad'inde; `a22an`,
bağımlılıksız ~1 850 satır Rust):** yakalanan kareleri kit'in istemci
kurallarıyla yürür (full değiştirir; delta `removed` → `cell_exits` →
upsert; private full koşulsuz), her kaydı oyunun alan düzeniyle (demo
`EntityRecord`, arena/MMO/savaş `UnitRecord`/`EntityRecord` — alan 1 id,
konum alanları, diğerleri) byte byte ayırır, her upsert'i GRUBUN o
kareden önceki değerine (TCP'de kayıpsız istemcinin görünümü = grubun
`held`'i) göre sınıflar, aday kodlamalarla aynı kareleri yeniden kodlar
(bugünkü protobuf yeniden kodlaması yakalanan baytla BİREBİR tuttu —
sekiz koşunun sekizinde sağlama), rUDP kaybını simüle eder ve
kodlama/çözme CPU'sunu ölçer (yuvarlak-dönüş denetimli). Prototip olduğu
için repoya alınmadı: uygulama turu gerçek kodlayıcıyı yazınca ölçüm
`--capture` + kit'in kendi `ClientView`'ıyla yapılır.

**Koşular** (release, 32 çekirdek, makine başka ajanların işleriyle
yüklü; `gsb-loadgen N --game G --duration 10 --write-stall-secs 0
--capture DIR --capture-clients 8`, demo için ayrıca `--visibility
spatial --topology sharded --shard-count 4`; N = 200, 500). Her koşuda
`joined = left = N`, `errors=0`, `server_closes=0`, fan-out `dropped=0`,
`server_hz` 29,99–30,01:

| Oyun (oda) | N | 1 dk yük | `out_bps_per_conn` | örnek istemci B/sn | kayıt/kare | en büyük kare (B) |
|---|---|---|---|---|---|---|
| demo (sharded × spatial, ring) | 200 / 500 | 18,5 / 14,4 | 11 617 / 27 941 | 12 558 / 29 029 | 32,5 / 79,6 | 1 519 / 3 483 |
| arena (takım sisi, delta) | 200 / 500 | 21,1 / 15,7 | 40 082 / 98 106 | 42 402 / 106 179 | 96,6 / 238,7 | 1 950 / 4 916 |
| MMO (sharded × spatial) | 200 / 500 | 17,4 / 17,8 | 21 433 / 50 768 | 22 182 / 54 216 | 38,3 / 95,4 | 893 / 2 258 |
| savaş (team × sharded, delta) | 200 / 500 | 14,9 / 15,8 | 67 488 / 178 611 | 69 027 / 193 023 | 105,8 / 292,8 | 3 468 / 9 118 |

("örnek istemci B/sn": yakalanan sekiz istemcinin baytı + kare başına
6 B TCP başlığı / yakalama süresi; sunucunun ölçtüğüyle uyumlu.)

#### Bayt anatomisi

Kare türleri (tüm baytların payı, 500'de; 200 aynı resim): grup
delta'ları **%87–96**, grup full'ları (taze + keep-alive) %3–7, private
one-shot full'lar demo'da %6 (AOI grup geçişleri; 200'de %9), diğerlerinde
≤ %1; ack/oturum kareleri ≤ %0,5. Yani bant = hareketli birimlerin delta
kayıtları. Kare içi (bütün karelerin toplamı):

| Oyun | N | TCP başlığı + zarf başlığı | kayıt çerçevesi (`0x12` + uzunluk) | wire id (alan 1) | konum alanları | diğer alanlar | `removed` + `cell_exits` | ort. kayıt gövdesi (id / konum / diğer) |
|---|---|---|---|---|---|---|---|---|
| demo | 200 | %3,0 | %18,8 | **%40,3** | %35,6 | — | %2,3 | 8,1 B (4,3 / 3,8 / 0) |
| demo | 500 | %1,4 | %20,0 | **%38,2** | %38,1 | — | %2,4 | 7,6 B (3,8 / 3,8 / 0) |
| arena | 200 | %0,9 | %16,3 | %19,2 | **%63,5** | — | %0,1 | 10,1 B (2,4 / 7,8 / 0) |
| arena | 500 | %0,4 | %16,2 | %22,2 | **%61,2** | — | %0,1 | 10,3 B (2,7 / 7,6 / 0) |
| MMO | 200 | %1,7 | %12,3 | %24,2 | %37,0 | %24,6 | %0,1 | 13,9 B (3,9 / 6,0 / 4,0) |
| MMO | 500 | %0,7 | %12,7 | %23,0 | %38,1 | %25,4 | %0,1 | 13,6 B (3,6 / 6,0 / 4,0) |
| savaş | 200 | %0,5 | %11,0 | %22,5 | %33,0 | %33,0 | 0 | 16,1 B (4,1 / 6,0 / 6,0) |
| savaş | 500 | %0,2 | %10,9 | %23,2 | %32,8 | %32,8 | 0 | 16,2 B (4,3 / 6,0 / 6,0) |

Üç şey konumdan bağımsız: **kayıt başına 2 B çerçeve** (%11–20),
**wire id** (%19–40) ve **konum dışı alanlar** (MMO/savaş %25–33).
Wire id'ler shard'lı oyunlarda 3–4 baytlık varint: shard `k`'nin id
aralığı `k · 2^20`'den başlıyor (`gsb_core::shard::SHARD_SERIAL_RANGE`,
kit'in `Minter::range`'i) — shard 1'in id'leri 21, shard 2–3'ünkiler 22
bit (savaş 500: kayıtların %55'inde id 4 B, %26'sında 3 B; demo 200'de
%64'ünde 4 B). Tek odalı arenada id 1–2 B. Konum dışı alanlar (MMO `kind`
+ `hp`, savaş `kind` + `faction` + `hp`) etiketleriyle kayıt başına 4–6 B
ve neredeyse hiç değişmiyor (aşağıda).

#### Değer değişimi — grubun son gönderdiğine göre

| Oyun | N | delta upsert'leri: yeni / yeniden giren¹ / değişmemiş / yalnız konum / konum dışı değişti | keep-alive + taze full'larda değişmemiş | private full'larda değişmemiş |
|---|---|---|---|---|
| demo | 200 | %3,4 / %5,5 / %0,9 / %90,2 / 0 | %58,5 | %78,1 |
| demo | 500 | %3,2 / %5,5 / %0,8 / %90,5 / 0 | %60,9 | %77,7 |
| arena | 200 | %0,7 / 0 / 0 / %99,3 / 0 | %10,3 | (hepsi yeni) |
| arena | 500 | %0,7 / 0 / 0 / %99,3 / 0 | %6,6 | (hepsi yeni) |
| MMO | 200 | %0,6 / %0,4 / 0 / %99,0 / %0,1 | %4,7 | %84,4 |
| MMO | 500 | %0,5 / %0,4 / 0 / %99,0 / 0 | %2,5 | %72,6 |
| savaş | 200 | %0,4 / 0 / 0 / %99,5 / %0,1 | %24,4 | (hepsi yeni) |
| savaş | 500 | %0,3 / 0 / 0 / %99,1 / %0,6 | %11,7 | %19,8 |

¹ AOI: aynı karede `removed`/`cell_exits` ile çıkıp hedef hücrenin
parçasında geri gelen kayıt (hücre geçişi) — paylaşılan hücre parçasında
mutlak kalmak zorunda (aşağıda).

Defterler zaten değişmeyeni göndermiyor (delta'larda değişmemiş ≤ %0,9);
"değişmeden yeniden gönderim" yalnız full'larda ve full'lar baytın
%3–16'sı. Delta'daki kayıtların **%99'u yalnız konumu değiştirmiş**.
Eksen başına |Δ| (wire birimi, delta'daki değişmiş kayıtlar):

| Oyun | birim | x p50/p90/p99 | y | z | zigzag(Δ) 1 B'ye sığan |
|---|---|---|---|---|---|
| demo | tamsayı birim | 1/1/1 (%45 = 0) | 1/1/1 | — | %100 |
| arena | cm | 20–28 / 39 / 41 (maks 42) | 17–19 / 33 / 40 | 6–11 / 31 / 37 | %100 (Δ ∈ ±63: 7 bit) |
| MMO | dm | 2/2/3 | hep 0 (yer) | 2/2/3 | %100 (Δ ∈ ±7: 4 bit) |
| savaş | dm | 1–2/2/3 | hep 0 | 1–2/2/3 | %100 (4 bitlik) |

(Ara sıra büyük sıçramalar — MMO `Travel`, savaş ölüm → üs — binlerce
dm; ≤ 7 dışında kalan oran %0,05'in altında.) Yani göreli kodlama bir konum eksenini 2
bayttan (etiket hariç) 1 bayta ya da yarım bayta indirir; kazanç
kayıttaki etiket, id ve çerçeve baytlarının yanında küçük kalır.

#### Aday kodlamaların yeniden oynatımı

Aynı kareler, kayıt gövdesi ve (varsa) zarf değiştirilerek yeniden
kodlandı; başlık, `removed`, `cell_exits`, private sarmalayıcı ve ack
kareleri bugünkü gibi sayıldı. Adaylar:

- **P0** bugün (protobuf gövde, kayıt başına `0x12` + uzunluk).
- **D1** etiketsiz varint gövde (id, sonra her alan zigzag/varint) —
  yalnız oyunun kodeği değişir.
- **D2** harita genişliğinde bit paketli gövde (arena x/z 14, y 12 bit;
  MMO 14/11/14 + kind 2 + hp 10; savaş 14/8/14 + 2 + 2 + 7; demo 8/8) —
  yalnız oyunun kodeği.
- **run** kit zarfında "paketli kayıt koşusu": kayıtlar tek bir `bytes`
  alanında art arda, kayıt başına çerçeve yok (kayıtlar kendini
  sınırlıyor) — kit zarfı değişir, anlam MUTLAK kalır.
- **origin** karenin konum kutusunun köşesi başlıkta, konumlar kutuya
  göre en dar bit genişliğinde (mutlak, idempotent).
- **kompakt id**: her wire id < 16 384 (2 B varint) — shard aralıklarını
  iç içe geçiren ya da küçülten bir id basımı.
- **D5** alan düzeyinde kısmi upsert (değişen alanlar, MUTLAK değerle;
  istemci birleştirir).
- **R1/R2** değer-göreli (a): kayıt = id + başlık baytı + değişen
  eksenlerin Δ'sı (R1 zigzag varint; R2 sınıflı: eksen başına 4 bit / 1 B
  / varint) + değişen diğer alanlar mutlak; baseline'ı olmayan ya da
  yeniden giren kayıt mutlak (D2 gövde + başlık). Full'lar mutlak.
- **(b)** R2 + run + karede `base_seq` (varint fark).
- **K1** anahtar kare göreli: Δ grubun son FULL'undaki değere göre.
- **(c)** bağlantı başına onaylı baseline (Quake 3 / Overwatch): kare =
  bağlantının 3 ya da 6 tick önce onayladığı görünümden farkı (kaydı o
  tarihten beri değişen her şey + çıkanlar `removed` olarak); keep-alive
  full'ları gerekmez.
- **A10** varlık başına yayın hızı: kayıt en çok her 2. (15 Hz) ya da 3.
  (10 Hz) tick'te yeniden gönderilir, bekleyen kayıt vadesi gelince o
  anki değeriyle (yeni ve yeniden giren kayıt hemen).

Bugüne göre bant (istemci başına B/sn değişimi; sekiz örnek istemci):

| Aday | demo 200 | demo 500 | arena 200 | arena 500 | MMO 200 | MMO 500 | savaş 200 | savaş 500 |
|---|---|---|---|---|---|---|---|---|
| P0 bugün (B/sn) | 12 558 | 29 029 | 42 402 | 106 179 | 22 182 | 54 216 | 69 027 | 193 023 |
| D1 etiketsiz (yalnız oyun) | −26 % | −28 % | −27 % | −26 % | −25 % | −25 % | −28 % | −27 % |
| D2 bit paketli (yalnız oyun) | −26 % | −28 % | −31 % | −29 % | −25 % | −25 % | −39 % | −38 % |
| D2 + run (kit zarfı) | −44 % | −48 % | −47 % | −45 % | −37 % | −38 % | −49 % | −49 % |
| **D2 + run + kompakt id** | **−58 %** | **−58 %** | **−47 %** | **−45 %** | **−44 %** | **−43 %** | **−57 %** | **−57 %** |
| D2 + run + origin | −44 % | −47 % | −47 % | −45 % | −48 % | −50 % | −54 % | −54 % |
| D5 kısmi upsert (mutlak) | −23 % | −25 % | −22 % | −21 % | −39 % | −41 % | −46 % | −46 % |
| R1 göreli varint (a) | −23 % | −25 % | −41 % | −40 % | −49 % | −51 % | −55 % | −55 % |
| R2 göreli (a) | −24 % | −26 % | −39 % | −36 % | −48 % | −49 % | −54 % | −54 % |
| R2 + run (a) | −42 % | −46 % | −55 % | −53 % | −60 % | −62 % | −65 % | −64 % |
| (b) = R2 + run + `base_seq` | −42 % | −46 % | −55 % | −52 % | −59 % | −62 % | −65 % | −64 % |
| R2 + run + kompakt id | −56 % | −56 % | −55 % | −53 % | −67 % | −67 % | −72 % | −72 % |
| savaş, ithal kayıtlar mutlak² (R2 + run / + kompakt id) | — | — | — | — | — | — | −50 % / −57 % | −50 % / −58 % |
| K1 anahtar kare göreli | −41 % | −45 % | −39 % | −37 % | −54 % | −55 % | −59 % | −59 % |
| (c) bağlantı başı, 3 tick gecikme | **+12 %** | **+10 %** | −50 % | −48 % | −57 % | −60 % | −57 % | −58 % |
| (c) bağlantı başı, 6 tick gecikme | +27 % | +23 % | −34 % | −32 % | −51 % | −53 % | −51 % | −52 % |
| A10 15 Hz, bugünkü gövde | −3 % | −3 % | −45 % | −46 % | −45 % | −46 % | −38 % | −40 % |
| A10 15 Hz + D2 + run | −46 % | −49 % | −70 % | −70 % | −65 % | −66 % | −69 % | −70 % |
| **A10 15 Hz + D2 + run + kompakt id** | **−59 %** | **−59 %** | **−70 %** | **−70 %** | **−68 %** | **−69 %** | **−73 %** | **−74 %** |
| A10 10 Hz + D2 + run | −53 % | −56 % | −79 % | −79 % | −75 % | −76 % | −78 % | −79 % |

² Savaşın görünümünün çoğu BAŞKA shard'ların kodladığı gövdeler
(`Shown::Encoded` — çekirdeğin takım rölesi `TeamRecord { bytes }` opak
bayt taşır): tipli değer olmadan Δ hesaplanamaz. Satır, izleyicinin
shard bölgesi + 200 m şerit dışındaki kayıtları (izleyicinin kendi
kaydının konumundan — yakalanan JOIN sonucu) mutlak sayar. Göreli
kodlama savaşta ancak kodeğe bir `decode` seam'i (ya da röleye tipli
değer) eklenirse "R2 + run" satırına iner.

Okuma:

1. **Mutlak sıkıştırma kazancın çoğunu alıyor.** Bugünkü protobuf
   gövdenin etiketleri + kayıt çerçevesi + şişkin id'ler baytın ~yarısı;
   "D2 + run + kompakt id" dört oyunda **−43 … −58 %** — ve istemci
   kurallarından hiçbiri değişmiyor (kayıt hâlâ mutlak, idempotent,
   kayba dayanıklı; encode-once aynen).
2. **Göreli kodlamanın bunun üstüne kattığı** (aynı zarf ve id'lerle):
   demo **−2 puan (daha kötü)** — Δ ±1 zaten 1 B, mutlak koordinat da 1 B,
   göreli başlık baytı fazladan; arena **+8**; savaş **+1** (ithaller
   mutlak kaldıkça; decode seam'iyle +15); MMO **+23** (AOI, ithal yok,
   `kind`+`hp` her kayıtta 4 B). Tek anlamlı kazanç MMO tipi oyunda.
3. **A10 (varlık başına hız) göreli kodlamadan büyük** ve onunla değil
   mutlak sıkıştırmayla birleşiyor: 15 Hz + D2 + run + kompakt id
   **−59 … −74 %**; savaş 1000'in ~365 KB/sn'si kabaca ~95 KB/sn olur
   (500'deki oranla ölçeklenmiş tahmin). Demo'da A10 bugünkü gövdeyle
   yalnız −3 %: tamsayı konum zaten 2–3 tick'te bir değişiyor.
4. **(c) bant olarak da kazanmıyor:** onaylı baseline 3–6 tick geride,
   Δ'lar ve gönderilen küme büyüyor (demo'da hücre çıkışları yerine id
   başına `removed` — **+10 … +27 %**); keep-alive'ı kaldırmasına rağmen
   R2 + run'dan kötü.
5. **origin** yalnız AOI MMO'da (+11–12 puan; 3×3 × 64 m blok) ve
   savaşta (+5) anlamlı; zarfa oyun anlamı (hangi alan konum) sokar.
6. **D5** (kısmi mutlak upsert) konum dışı alanları olan oyunlarda D2'ye
   yakın/iyi (MMO −40 %, savaş −46 %), arenada kötü (maske baytı).

#### Kayıp: rUDP oyun bandında

Bağımsız datagram kaybı (%1, %5); 1 472 B üstü kare FRAG'a bölünür ve
herhangi bir parçası kaybolursa kare kaybolur (DESIGN §6 "MTU"). Her
istemci akışı × 20 tohum; "bayat" = istemci görünümünde kayıpsız
görünümden farklı ya da eksik/fazla kayıt, tick başına görünümün yüzdesi
olarak ortalama. "Koşu" = görünümün yarısından fazlası yanlışken art arda
geçen tick'ler.

| Politika | kare kaybı %1 → bayat (koşu) | kare kaybı %5 → bayat (koşu) |
|---|---|---|
| bugün: mutlak, boşluk olsa da uygula (P0 boyları) | arena 500 %3,0 (1,0) · MMO 500 %2,0 (1,1) · savaş 500 %4,7 (1,1) · demo 500 %1,1 | %14,3 · %9,8 · **%21,7** (1,3) · %5,7 |
| aynı kurallar, D2 + run + kompakt id boyları | %1,9 · %1,1 · **%2,3** · %0,9 | %9,4 · %5,2 · **%11,1** · %5,1 |
| (a) göreli, baseline denetimi yok | %19,6 (17 tick) · %12,4 · %20,8 (18) · %3,9 — **yanlış değerler** | %59 · %43 · %62 · %18 |
| (b) `base_seq` kapılı, sonraki full'a kadar düşür | %22,9 (17,5) · %14,3 · %25,1 (18) · %10,9 | %67 · %48 · %69 · %42 |
| (b) + yeniden senkron isteği (RTT 3 tick) | %5,8 (3,8) · %3,5 · %6,4 (3,9) · %2,5 | %24 · %16 · %26 · %12 |
| K1 anahtar kare göreli | %3,9 (2,0) · %2,3 · %5,4 (2,7) · %2,3 | %17 · %9,7 · %24 · %9,9 |

(200'lük koşular aynı sıralamayı veriyor; tam tablo scratchpad'de.)

- Bugünkü kurallar kaybı zaten iyi taşıyor: bir kayıp ≈ bir tick'lik
  bayatlık, çünkü hareketli birimler bir sonraki delta'da yeniden mutlak
  gelir. **Göreli kodlama bunu bozuyor:** kaybolan delta sonrası her
  delta yanlış tabana uygulanır ((a): sessizce yanlış konumlar,
  keep-alive'a — 1 sn'ye — kadar birikir) ya da düşürülür ((b): görünüm
  keep-alive'a kadar donar, ~16–26 tick). Yeniden senkron isteği bunu
  bugünkünün ~2 katına indiriyor ama istemci → sunucu yeni bir mesaj ve
  kayıp başına bir one-shot full demek. K1 en dayanıklı göreli biçim
  (yalnız anahtar karenin kaybı dondurur) ama bandı Δ büyüdükçe erir
  (arena −37 %).
- **Küçük kare = az kayıp:** mutlak sıkıştırma kareyi yarıya indirince
  FRAG parça sayısı düşüyor; savaş 500'de %1 datagram kaybında kare kaybı
  %4,8 → %2,4, %5'te %22 → %11. rUDP'de mutlak yol bandı VE bayatlığı
  birlikte düşürüyor.

#### CPU

Grup delta kareleri üzerinde (beş turun en küçüğü; makine yüklü,
±%20 gürültü), ns/kare (ns/kayıt):

| | demo 500 | arena 500 | MMO 500 | savaş 500 |
|---|---|---|---|---|
| kodlama, bugün (P0) — grup başına | 1 235 (14,1) | 3 811 (13,3) | 2 069 (18,1) | 7 030 (20,2) |
| kodlama, D2 + run | 1 130 (12,9) | 3 835 (13,4) | 2 311 (20,2) | 6 939 (19,9) |
| kodlama, R2 + run + defterden baseline araması | 2 508 (28,6) | 9 036 (31,6) | 3 940 (34,5) | 13 334 (38,2) |
| (c) bağlantı başına: görünümün tamamını onaylı görünümle karşılaştır + kodla | 11 224 (202 kayıt; 55,5) | 16 341 (304; 53,8) | 9 760 (121; 80,7) | 27 120 (442; 61,4) |
| çözme, bugün (P0) — istemci karesi başına | 2 591 (29,6) | 9 318 (32,6) | 4 017 (35,2) | 13 872 (39,8) |
| çözme, R2 + run (görünümdeki baseline'a Δ) | 2 745 (31,3) | 8 109 (28,3) | 2 969 (26,0) | 10 837 (31,1) |

- (a)/(b)'nin kodlaması kayıt başına ~2×, ama buradaki ölçüm her kayıtta
  bir hash araması içeriyor; gerçek defterde eski değer elde: `SetLedger::
  emit_delta`'nın `Entry::Occupied` kolu, `CellBook`'ta hücre içi
  güncellemede kovanın önceki değeri. Grup başına kodlandığı için (savaş
  shard'ında ≤ 3 takım karesi) tick başına onlarca µs — önemsiz.
- İstemci tarafı **ucuzlar** (bayt az, etiket yok): R2 çözümü P0'dan
  %10–25 hızlı (demo eşit). A24'ü (istemci delta maliyeti) kötüleştirmez.
- **(c) encode-once'ı bozuyor ve 1000'de sığmıyor:** maliyet bağlantı
  başına görünümün tamamı. Savaş 1000 (görünüm ~956, W2): ~61 ns × 956 ≈
  58 µs/bağlantı/tick → 1000 bağlantıda ~58 ms/tick, dört shard aktörüne
  bölünse shard başına ~14,6 ms (33 ms'lik tick'in %44'ü); bugün aynı iş
  shard başına ≤ 3 takım karesi ≈ ≤ 60 µs (~250×). Arena 1000 (tek oda
  aktörü, takım görünümü ~650): ~54 ns × 650 × 1000 ≈ 35 ms/tick — tick
  bütçesinin üstünde. Artı bağlantı başına onaylı durum geçmişi (varlık
  başına son onaylı değer tablosu bile ~1000 × 956 kayıt) ve onay yolu
  (bugün istemci snapshot'ı onaylamıyor; `InputAck` ters yön).

#### Seçenekler — istemci kuralı, seam, taşıma, shard

**(a) Grubun son gönderdiğine göre göreli, encode-once korunur.**
- *İstemci kuralı:* "baseline'lı delta boşluk olsa da uygulanır" DÜŞER:
  göreli kayıt yalnız grubun bir önceki karesini uygulamış istemcide
  doğrudur; kaybolmuş kareden sonraki her göreli kayıt yanlış değer
  üretir ve istemci bunu SEZEMEZ (sequence boşluğu olay güdümlü akışta
  normal). Bu yüzden (a) tek başına güvensiz: yalnız her üyenin grubun
  tabanını tuttuğu garanti edilirse doğru.
- *Garanti nerede kırılıyor:* rUDP oyun bandı (kayıp + FRAG); TCP/WS/
  TLS/QUIC güvenilir ama **çekirdeğin fan-out'u dolu çıkış kanalında
  batch'i düşürüyor** (`dropped_frames`; T turunda arena 1000'de
  1 000–1 800) — "güvenilir taşıma" ≠ "her kare ulaştı". (a) bu yüzden
  çekirdekten kit'e "şu oyuncunun batch'i düştü" sinyali ister (`GameLogic`
  kancası → kit `Baselines::forget` → sonraki tick one-shot full).
- *Seam:* `RecordCodec::encode_delta(id, old: &Wire, new: &Wire, out) ->
  bool` (varsayılan `false` = mutlak yaz), `ClientDecoder::record_delta(body,
  &mut Record)`; kit eski değeri defterden verir. İthal gövdeler için
  `RecordCodec::decode(body) -> Option<(u64, Wire)>` ya da röleye tipli
  değer (çekirdek opak; kit-içi değer tablosu gerekir).
- *AOI hücre parçaları:* parça bir kez kodlanıp hücreyi gören her gruba
  paylaşılıyor; hücre içi güncelleme herkeste aynı tabana dayanır (hücreyi
  gören her grup onu önceki tick'te de gördü — grup = sabit hücre bloğu,
  blok değişen bağlantı one-shot full alıyor), ama **hücreye GİREN kayıt**
  (geçiş; demo'da delta upsert'lerinin %5,5'i) mutlak kalmak zorunda:
  kaynak hücreyi görmeyen gruplar tabanı tutmuyor.
- *Göç / ödünç / ithal:* sahiplik değişince (kendi → ödünç, ithal →
  kendi, `Shown::Encoded` ↔ `Typed`) defterdeki değerin türü değişir →
  o kayıt bir kez mutlak. Göç eden oyuncunun kendisi zaten one-shot full
  alıyor (yeni grup).
- *Keep-alive / one-shot full:* mutlak kalır, tabanı sıfırlar.

**(b) Kare tabanını adlandırır (`base_seq`), istemci yalnız o tabandaysa
uygular.**
- *İstemci kuralı:* delta, istemcinin o gruptaki son kabul edilmiş
  sequence'ı `base_seq`'e eşitse uygulanır; değilse baseline'sız delta
  gibi düşürülür ve bir sonraki full'u bekler (ya da yeniden senkron
  ister). Bugünkü "boşluk olsa da uygula" kuralının yerine geçer; boşluk
  artık tespit edilebilir kayıptır.
- *Sunucu:* grup başına son gönderilen sequence (defter zaten tutuyor:
  `SetLedger` adımı; AOI için grup başına son kare tick'i). One-shot
  full'un sequence'ı = grubun o tick'teki karesi → sonraki delta'nın
  tabanıyla eşleşir (G3-2 sırası korunur).
- *Taşıma:* TCP'de fan-out düşmesi kendiliğinden yakalanır (istemci
  düşürür), ama iyileşme keep-alive'a kadar ~1 sn donmuş görünüm; rUDP'de
  %1 kayıpta görünümün ~%14–25'i bayat (tablo) — yeniden senkron isteği
  (istemci → sunucu yeni kontrol mesajı, sunucuda `Baselines::forget`)
  olmadan oyun bandında kullanılamaz.
- *Seam:* (a) ile aynı + zarfta `base_seq` alanı + istemcide grup başına
  son kabul sequence'ı (bugün zaten var).

**(c) Bağlantı başına onaylı baseline.** İstemci kuralı en temiz
(taban her zaman istemcinin onayladığı kare; kayıp yalnız Δ'yı büyütür),
ama encode-once gider, istemci → sunucu snapshot onayı gerekir (yeni
mesaj, rUDP'de oyun bandı), sunucuda bağlantı başına geçmiş tutulur; CPU
1000'de tick bütçesini aşıyor ve bant (a)'dan kötü. **Elendi.**

**(d) Göreli değil, daha sıkı mutlak kodlama.** Üç bağımsız parça:
- *(d1) Oyunun kodeği* (kit değişmez): etiketsiz/bit paketli gövde
  (−25 … −39 %). Kit sözleşmesi buna zaten izin veriyor (gövde opak
  bayt; `Wire = Bytes` de mümkün) — ama oyunun `.proto`'sundaki tipli
  ayna artık kaydı çözemez: ayna `repeated bytes entities` bildirir,
  istemci gövdeyi elle çözer (loadgen'in çözücüleri zaten
  `client::wire::Fields` ile elle yürüyor). Karar gerekir (soru 1).
- *(d2) Kompakt wire id* (çekirdek/kit id basımı; istemci kuralı
  değişmez, id opak): shard aralığı `k · 2^20` yerine iç içe geçmiş basım
  (`n · S + k`) ya da daha dar aralıklar → id ≤ 2 B; demo/MMO/savaşta
  +7 … +14 puan. Çekirdeğin aralık tükenme koruması (`serial_range`) ve
  id'leri sabitleyen testler etkilenir.
- *(d3) Paketli kayıt koşusu* (kit zarfı; anlam aynı): kayıt başına
  `0x12` + uzunluk yerine tek bir `bytes` alanında art arda kendini
  sınırlayan kayıtlar; +11 … +19 puan. İstemci kuralı yalnız "kayıtlar
  alan 2'de ya da alan 6'da" diye genişler (aşağıda önerilen metin).
- *(origin)* AOI'de ek +5 … +12 puan, ama zarfa "konum alanı" bilgisini
  sokar; ertelenir.

**(e) Birleşimler.** Ölçülen en iyi mutlak yol "D2 + run + kompakt id"
(−43 … −58 %); en iyi göreli "R2 + run + kompakt id" (−53 … −72 %,
savaşta decode seam'iyle); ikisinin üstüne A10 15 Hz (−59 … −74 %,
mutlak). "Her yerde (d), yalnız güvenilir taşımada (b)" birleşimi ölçülen
farkı yalnız MMO tipi AOI oyununda (+23 puan) haklı çıkarıyor; arenada
+8, savaşta decode seam'i olmadan +1, demo'da eksi.

#### Öneri

**A22'yi (değer-göreli kodlama) şimdi yapmayın.** Veri, kazancın
çoğunun göreli kodlamadan değil bugünkü kaydın şişkinliğinden geldiğini
gösteriyor: protobuf etiketleri, kayıt başına çerçeve ve 3–4 baytlık
shard id'leri. Bunlar kaldırılınca göreli kodlamanın ek kazancı −2 …
+8 puana iniyor (MMO hariç: +23), buna karşılık istemci sözleşmesinin kalbi —
mutlak, idempotent, kayba dayanıklı upsert — gidiyor ve yerine taban
takibi, yeniden senkron mesajı ve çekirdekten düşme sinyali geliyor.
Sıra (her adım ayrı karar, ayrı tur, her biri kendi başına ölçülür):

1. **(d2) Kompakt wire id** — istemci kuralı değişmez; bugünkü
   gövdeyle tek başına demo −10 … −14 %, MMO −6 … −7 %, savaş −7 %
   (arena 0: tek oda, id'ler zaten küçük).
2. **(d3) Paketli kayıt koşusu, oyun başına opt-in** — kit zarfı + iki
   küçük seam (aşağıda); (d1) oyunun kodeğinde (demolar için örnek
   kodek). Birlikte −43 … −58 %, rUDP'de kare kaybı yarıya.
3. **A10 varlık başına yayın hızı** — en büyük sonraki kaldıraç (15 Hz:
   arena/MMO/savaşta +17 … +26 puan; demo +1 — tamsayı konumu zaten
   seyrek değişiyor), istemci enterpolasyonu ister (Unity tarafı).
4. **A22 ancak bundan sonra ve bir MMO tipi oyun hâlâ bant sıkıntısı
   gösterirse**, (b) biçiminde: oda başına opt-in, yeniden senkron
   isteği + çekirdek düşme sinyaliyle, ithaller için `decode` seam'iyle.
   (a) tek başına (sessiz yanlış değer) ve (c) (CPU, bant) elendi.

*Önerilen `kit.proto` istemci kuralı metni (adım 2; `kit.proto`'ya bu
turda DOKUNULMADI):*

```proto
  // Entity records: in full mode the complete view, in delta mode the
  // upserts. Each entry is one record BODY written by the game's
  // `RecordCodec::encode` (the demo: an `EntityRecord`).
  repeated bytes entities = 2;
  ...
  // RECORD RUN (a game's opt-in, `RecordCodec::RUN`): the same records
  // as `entities`, written back to back in ONE field instead of one
  // length-delimited entry each — every record in the game's
  // self-delimiting run format (its client decoder reads one record off
  // the front of the run and knows where it ends). A frame carries its
  // records in `entities` OR in `records`, never both. The records keep
  // every rule of `entities`: in full mode the complete view, in delta
  // mode absolute, idempotent upserts applied after `removed` and
  // `cell_exits`, even across a sequence gap.
  bytes records = 6;
```

Kural bölümüne tek cümle: "`entities` in the rules below means the
frame's records, whichever field carries them (`entities` = 2 or the
`records` run = 6)." Başka hiçbir istemci kuralı değişmez.

*Seam (adım 2):* `RecordCodec` — `const RUN: bool = false;` (true: kit
kayıtları alan 6'ya art arda yazar, `encode` kendini sınırlayan kayıt
yazar; ithal gövdeler aynı biçimde olduğu için olduğu gibi eklenir);
`ClientDecoder` — `fn run_record(&self, run: &mut &[u8]) -> Result<(u64,
Self::Record), ClientError>` (varsayılan: `Malformed("no record run")`);
`ClientView`'ın ikinci yürüyüşü alan 6'yı bununla tüketir. `SetLedger` /
`CellPieces` / `put_entity_body` yalnız çerçeve yazımını değiştirir
(yazıcı `WriteRecord` üstünden, bayt baytına `RUN = false`'da bugünkü).
Tek dikkat: paylaşılan hücre parçaları bugün kayıt başına çerçeveli
bayt parçaları; run modunda parça çerçevesiz kayıt dizisi olur ve grup
karesi parçaları tek alan 6'nın gövdesinde birleştirir (uzunluk önekini
en sona yazmak için ölçü önce toplanır — `put_delimited`'ın tekniği).

*(b) seçilirse önerilecek metin (kayıt için):* "DELTA frames carry
`base_seq` (field 7): the sequence of the group frame (or one-shot
private full) the delta was encoded against. A client applies a delta
only if its last accepted sequence for the group equals `base_seq`;
otherwise it drops it as it drops a delta without a baseline, and heals
at the next full (group full, keep-alive full or the one-shot full a
resync request (op …) obtains). Records flagged relative are the
difference from the value the client holds; a relative record for an id
the client does not hold is a protocol error." — ve "a delta WITH a
baseline is applied on top, even across a sequence gap" cümlesi silinir.

#### Bakımcının karar vermesi gerekenler

1. **Oyun kayıt gövdesi protobuf'tan çıkabilir mi?** (d1) tipli aynanın
   kaydı çözmesini bırakır (ayna `repeated bytes`, gövde elle). Kabulse
   dört demo için örnek bit paketli kodek bir turun işi. Değilse (d3)
   de anlamını yitirir (protobuf gövdesi kendini sınırlamaz, koşuda yine
   uzunluk ister) ve kalan yalnız (d2): −43 … −58 % yerine −0 … −14 %
   (ölçüldü: "P0 + kompakt id" demo −14/−10, arena 0, MMO −7/−6, savaş
   −7/−7).
2. **Kit zarfına yeni alan (`records = 6`) — sürüm nasıl?** Oyun başına
   opt-in (eski istemci o oyunda yeni alanı bilmezse kayıtları görmez);
   E5 (protokol sürüm müzakeresi) şimdi mi gerekli, yoksa oyun
   opcode'u/`protocol_version` yeter mi?
3. **Wire id basımı değişebilir mi?** (iç içe geçmiş ya da dar aralık;
   çekirdeğin `SHARD_SERIAL_RANGE`/tükenme koruması, id'leri sabitleyen
   testler.) Id istemciye opak; göçte id korunur, bu değişmez.
4. **A10'un önceliği** — Unity tarafında enterpolasyon taahhüdü var mı?
   Varsa sıra 1 → 2 → A10; yoksa 1 → 2 ve A10 bekler.
5. **A22 BACKLOG'da nasıl kalsın?** Öneri: tetikleyicisi "(d)+A10
   sonrası hâlâ bant/MTU baskısı gösteren AOI tipi bir oyun" olarak
   yeniden yazılsın; biçim (b) + yeniden senkron + düşme sinyali.

**Bulgular (kayıt):**

- **A22-1 — fan-out düşmesi one-shot full'u da düşürüyor.** Çekirdek dolu
  çıkış kanalında batch'in tamamını atıyor; o batch'te one-shot full
  varsa `Baselines` oyuncuyu baseline'lı sayar, istemci keep-alive'a
  kadar delta'ları düşürür (bugünkü kurallarla güvenli: en çok bir
  keep-alive). Göreli her seçenekte bu bir doğruluk sorunudur → düşme
  sinyali gerekir. Bugün kod değişmedi.
- **A22-2 — shard id'leri kaydın ~%23–40'ı.** `k · 2^20` aralıkları shard
  1–3'ün id'lerini 3–4 baytlık varint yapıyor; tek odalı arenada yok.
- **A22-3 — keep-alive/private full'lar değişmemiş kayıt taşıyor ama
  baytın ≤ %16'sı:** keep-alive temposunu oynatmak kaldıraç değil.
- Sayılar ve politika simülasyonu 32 çekirdekli, yüklü (1 dk 14–21)
  makinede, loopback TCP; rUDP kaybı simüle (gerçek ağ değil).

**Doğrulama:** 818 → **821** test / 0 hata / 1 ignored (+2 `capture`
birim testi, +1 `loadgen_games` yakalama testi; args testine bir ret
satırı); `cargo clippy --workspace --all-targets -- -D warnings` 0;
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` temiz;
hiçbir bayt sabitleme testine dokunulmadı.

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

**Durum (Faz 4 sonunda).** (1) Sağlandı: 2D demo, 3D arena ve 3D MMO
ayrı crate'ler, yalnız kit'in public yüzeyiyle, Faz 2'den beri
değişmemiş aynı kit üzerinde yeşil (§10 "Faz 4 sonucu", kapanış
kontrolü); kit'te değişiklik gerektiren her ihtiyaç tasarım bulgusu
olarak kayıtlı (§10, "Faz 3 + Faz 4 tasarım bulguları" — biri, F1, bir
doğruluk hatası; düzeltmesi sıradaki kit turunun işi). (2) Sağlandı:
2D demo'nun wire testleri yeşil ve beklenen baytları hiç değişmedi
(`wire_contract.rs`'e Faz 2'de yalnız crate yolu ve `prelude` import'u
dokundu; `kit_wire.rs` kit zarfını demo'nun aynasına sabitliyor). (3) Son A/B Faz 2'de alındı (gürültü
içinde); Faz 3 ve 4 çalışma zamanında kod değiştirmedi, yalnız loadgen
sağlamlık koşusu. (4) Sağlandı: `crates/gsb-core` Faz 0'dan beri
dokunulmadı.

**Son durum (Faz 5 sonunda — kabul).** Dördü de sağlandı:
1. **Üç demo aynı kit'i kullanıyor:** 2D demo, 3D arena, 3D MMO ayrı
   crate'ler, yalnız kit'in public yüzeyiyle; demoların kaydettiği her
   tasarım bulgusu kit'te kapandı (§10 "Faz 3 + Faz 4 tasarım
   bulguları", hepsi **çözüldü**; "Faz 5 sonucu"). Kanıt: kapanış
   kontrolü `cargo test -p gsb-demo -p gsb-demo-arena -p gsb-demo-mmo`
   → 83 passed / 0 failed; kit tarafında her düzeltmenin önce kırılan
   testi ve mutation-check'i.
2. **Mevcut istemciler değişmeden çalışıyor:** 2D demo'nun wire
   baytları aynı — `wire_contract.rs`, `kit_wire.rs`, `delta_aoi.rs`,
   kit'in `aoi/tests/sharing*`'i, arenanın ve MMO'nun `wire.rs`'i Faz
   5'te dokunulmadan yeşil (`git diff 32118b2.. --stat` bu dosyalarda
   boş); A3'ün kaldırdığı varsayılanlar demo'da zaten açıkça bildirilen
   1003/1004'tü.
3. **Performans gürültü içinde:** Faz 5 loadgen A/B'si (`32118b2` ↔
   `2d39767`, dört senaryo, dönüşümlü üçer çift — §10 "Faz 5 sonucu");
   Faz 1 ve 2'nin A/B'leri yeniden düzenleme öncesine karşı.
4. **`gsb-core` dokunulmadı:** `git diff 32118b2.. --stat --
   crates/gsb-core` boş; çekirdek Faz 0'dan beri aynı.

**W2 (2026-09-25) — dördüncü doğrulama oyunu.** Kriter 1 dördüncü bir
görünürlük modeliyle yeniden sınandı: "Cephe" (`gsb-demo-war`, §10 "W2
sonucu") W1'in `team × sharded` kompozitini kit'e dokunmadan kullandı;
bulguları (W2-1…W2-4) kod değişikliği olmadan kayıtlı. Kriter 2: mevcut
oyunların bayt kilitleri dokunulmadan yeşil. Kriter 4'ün (çekirdek)
yerini W1'den beri çekirdek seam'leri aldı; W2 çekirdeğe yalnız ölçüm
için dokundu (takım sayaçları metrik yolunda, A26).

## 12. Kararlar (kullanıcı, 2026-09-24)

1. **İsimler:** `gsb-kit` ve `gsb-demo` (`gsb-game` yeniden adlandırılır).
2. **§8'deki açıklar Faz 1'de** kapanır — her biri önce davranış testiyle
   kanıtlanır, sonra ayrı commit'le düzeltilir.
3. **Üç kontrol demosu** (Faz 3 ve Faz 4):

| Demo | Crate | Konum / wire | Sınadığı kit yüzeyi |
|---|---|---|---|
| 2D (mevcut) | `gsb-demo` | `Pos2` f32 sim, `(i32,i32)` wire | **bayt uyumluluğu** (mevcut istemciler, loadgen), tüm stratejiler |
| 3D arena | `gsb-demo-arena` | `Pos3<f32>`, 3D wire | **savaş sisi / takım görüşü** (MOBA tarzı: bir takım, üyelerinden herhangi birinin gördüğünü görür); `Vision` seam'i 3D mesafeyle, görüş-komşuluk ızgarası olarak `Grid3` (27 hücre) *(Faz 2: bu ızgara `VisionGrid3` adıyla kuruldu — `Vision`'ın 3D ön-ayarı; `CellSpace` olan hacimsel AOI `Grid3` değil, §7)*; 2'den fazla takım (sabit-2 açığının kapandığını sınar); küçük oda, hızlı hareket |
| 3D MMO | `gsb-demo-mmo` | `Pos3<f32>`, yer-düzlemi hücre | **grid AOI + shard'lı dünya**: sharded × spatial kompoziti, 3D konumda **yer-düzlemi** `Partition` + `Grid2` AOI, oyun kodunun spawn/despawn ettiği NPC'ler (hayalet-entity düzeltmesini sınar), park/bot reconnect politikası, NPC göçü (`Speed`'siz entity açığını sınar) |

Arena ve MMO bilinçli olarak farklı **görünürlük modelleri** sınar
(kullanıcı kararı): arena takım bazlı görüşü (`Vision`), MMO uzamsal
ızgara + sharding'i (`CellSpace` + `Partition`). Uzay tarafında da
farklılar: arena 3D veriyi hacimsel ızgarayla (`Grid3`; Faz 2'de
`VisionGrid3` olarak kuruldu) işler, MMO yer
düzlemine yansıtır (`Grid2` + `GridPartition2`). Böylece ön-ayarların 3D
veriyle iki farklı kombinasyonu da sınanır.

**Kapsam dışı (bilinçli):** `gsb-server`'ın ve loadgen'in oyundan
bağımsız yapılması (§9). *(Bu tasarımın dışında kaldı, sonradan
`docs/GAME-MODULE.md` G1–G4 ile yapıldı: sunucu ve loadgen üç demoyu da
barındırıp sürüyor; aşağıdaki iki cümle Faz 3/4 dönemini anlatır.)* Sunucu ikilisi ve loadgen 2D demo'ya bağlı kalır;
3D demolar kendi crate testleriyle (oda aktörü üzerinden, gerçek
`GameLogic` yolu) doğrulanır. Faz 4 sonunda, her demonun uçtan uca
çalışabilmesi için gereken en küçük sunucu kancası ihtiyacı ayrıca
değerlendirilir.

## 13. Faz 4 — MMO demosu (kapanış doğrulaması)

Faz 3'ün (3D arena) ardından gelir. Amaç, "her şey yerine oturdu mu"
sorusunu en ağır kullanım senaryosuyla cevaplamaktır. MMO demosu kit'te
bir değişiklik gerektirirse, bu turun raporu o değişikliği ve gerekçesini
ayrıca listeler; kabul kriteri 1'e göre bu bir tasarım bulgusudur.

*(Tamamlandı: §10 "Faz 4 sonucu" — MMO kit'e dokunmadan kuruldu; kit
değişikliği gerektiren dört ihtiyaç tasarım bulgusu olarak orada
listeli, Faz 3'ünkülerle birlikte "Faz 3 + Faz 4 tasarım bulguları"
tablosunda.)*
