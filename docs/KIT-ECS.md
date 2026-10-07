# gsb: Kit'in ECS Bağımsızlığı — Tasarım (tur k0)

> **Durum:** TASARIM — kullanıcı incelemesi bekliyor. Uygulama YOK; bu
> turun çıktısı bu doküman ve `spikes/k0-kit` (gsb çalışma alanının
> üyesi olmayan, kendi küçük çalışma alanı olan, motoru değiştirmeyen bir
> kanıt: ECS'siz çekirdek + bevy_ecs bağlayıcısı + hecs bağlayıcısı +
> eşitlik testleri + ölçüm). Motor crate'lerine dokunulmadı; kökteki
> `Cargo.toml`'a yalnız `exclude` girdisi eklendi.
>
> Kod ve yorumlar İngilizce'dir; bu doküman Türkçe'dir (repo sözleşmesi).
> "Bevy" bu dokümanda **yalnız `bevy_ecs` crate'i** demektir — Bevy oyun
> motoru hiçbir yerde yoktur ve olmayacaktır.

**Tek paragraf özet.** Kit bugün `bevy_ecs`'i doğrudan kullanıyor (197
dosyanın 96'sı; test dışı 77 dosyanın 48'i, ~9.6k satır). Ama envanter
(§3) gösteriyor ki **işin kendisi zaten ECS'siz**: hücre-delta motoru
(`CellBook` ilkelleri, `CellPieces`, paket birleştirme), küme defterleri,
taban çizgileri, girdi sırası, park defteri, çerçeveler, hız sınıfları,
istemci görünümü — hepsi düz veri üstünde. ECS'e dokunan yerler **yedi
dikişte** toplanıyor: kayıt kaynağı (hangi varlıklar, tel değerleri),
değişim kaynağı (`Changed<T>`), çıkış kaynağı (silinen-bileşen
tamponları), varlık yaşam döngüsü (spawn kancası, `WireId` damgası,
despawn), stratejilerin varlık başına okudukları (takım, görüş yarıçapı,
konum, ışık), dünyada tutulan kit durumu (`KitKicks` kaynağı, `Disabled`)
ve oyun kancalarının imzaları (`&mut World`, `Entity`). Öneri: `gsb-kit`
**ECS'siz** kalır ve bu dikişleri tek bir bağlayıcı sözleşmesiyle
(`Ecs` + `RecordFeed`) ifade eder; varlık **opak `Owner(u64)`** olarak
görünür (SERVICES §5.5'in fikri). `gsb-kit-bevy-ecs` ve `gsb-kit-hecs`
ince bağlayıcılardır: girdiyi toplar, çıktıyı uygular, ECS'e özgü
optimizasyonu (bevy'nin kalıcı `QueryState`'i) taşır. Kanıt: AOI'nin
sıcak yolu (`AoiRoom::update`'in hücre defteri kısmı) çekirdeğe taşındı;
aynı senaryo bugünkü bevy-yerli yol, çekirdek+bevy_ecs ve çekirdek+hecs
üstünde **her tick aynı defter durumunu** ve bağlayıcılar arasında **bayt
bayt aynı paketleri** üretiyor; çekirdek+bevy_ecs bugünkü yolla ölçüm
gürültüsü içinde aynı hızda, çekirdek+hecs (`Tracked<T>` + günlüklü
dünya) de öyle (§8).

---

## 1. Kural (kullanıcı kararı — yeniden açılmaz)

> "Kit tüm ECS'lerde kullanılabilir parçalar olmalı. Tüm işlevlerimiz ECS
> bağımsız olmalı. ECS bağımlılıkları bağlayıcı paketlerde olmalı."

- Motorun **her** işlevi, `gsb-kit` dahil, ECS'ten bağımsızdır.
- ECS bağımlılıkları (`bevy_ecs`, `hecs`, `shipyard`) **yalnız** ince
  bağlayıcı paketlerde yaşar (`gsb-kit-bevy-ecs`, `gsb-kit-hecs`, …);
  istenirse bir bağlayıcı/şemsiye crate'in `bevy_ecs` özelliği arkasında
  yeniden dışa aktarılır. İşlevsel kod hiçbir ECS'i içe aktarmaz.
- Çekirdek zaten ECS'siz (`GameLogic<W>` jeneriktir); istek tasarımı
  (SERVICES §5.5) opak `Owner(u64)` + ECS başına `.entity()` /
  `.to_owner()` uzantılarıyla aynı fikri kullanıyor — burada yeniden
  kullanılır.

## 2. Bugün: bağımlılık haritası

| Crate | `bevy_ecs` | Not |
|---|---|---|
| `gsb-core` | yok | `GameLogic<W>`, `RoomLogic<W>`, `ShardLogic<W>` dünya tipine jenerik; çekirdek dünyaya hiç bakmaz. |
| `gsb-ecs` | var (tek işi) | El yazması `System` trait'i + `SystemRunner` (DESIGN §7). Tek kullanıcısı `gsb-demo`. |
| `gsb-kit` | **var, koşulsuz** | Bu turun konusu (§3). |
| `gsb-server` | **var, koşulsuz** | Yalnız `games/{demo,arena,mmo,war}` modülleri kullanıyor — hepsi zaten `game-*` özellikleri arkasında. `lib.rs`'te yalnız bir yorum. `gsb-ecs` bağımlılığı **hiç kullanılmıyor** (src'de `gsb_ecs` yok). |
| `gsb-demo`, `-arena`, `-mmo`, `-war` | var | Doğrulama düzenekleri (bileşenler, sistemler, `RecordCodec` örnekleri). |
| `gsb-net`, `gsb-protocol`, `gsb-client`, `gsb-ticket`, `examples/lobby` | yok | — |

Sunucunun oda fabrikaları bugün `RoomFactory<World, …>` ile somut
`bevy_ecs::World` kuruyor; `GameModule`/kayıt defteri zaten `W`'ye
jenerik (`spawn<W, G, St, Sp>`), yani sunucunun çekirdeği hecs dünyasını
olduğu gibi barındırabilir.

## 3. Envanter: her kit modülünün ECS dokunuşları

Sınıflar:
**(A) saf algoritma** — hiç ECS yok ya da yalnız tip adı (`Entity` anahtar);
**(B) veri beslemesi gerekir** — her tick ya da isteğe bağlı olarak
varlıkların bileşenlerini okur (tam geçiş ya da tek varlık);
**(C) varlık yaşam döngüsü gerekir** — spawn/despawn, bileşen ekleme/
çıkarma, varlığı kimlikle eşleme;
**(D) değişim algılama gerekir** — "bu tick ne değişti / ne gitti".

| Modül | Okur / yazar | Değişim algılama | Önbellek | Yaşam döngüsü | Sınıf |
|---|---|---|---|---|---|
| `codec.rs` (`RecordCodec`) | dikişin tanımı: `Marker: Component`, `Query: ReadOnlyQueryData + SingleEntityQueryData + ReleaseStateQueryData`, `Dirty: QueryFilter`, `wire(QueryItem)` | `Dirty` filtresi oyunun | — | — | B+D (dikiş) |
| `codec/rate.rs` (`SendEvery`) | — | — | — | — | A |
| `identity.rs` | `WireId` `#[derive(Component)]`; `Minter` düz | — | — | damga bileşen olarak | A (+C: bileşen) |
| `game.rs` (`Game`, `TeamGame`, `ShardGame`) | her kanca `&mut World` / `&World` alır, `Entity` döner/alır; `players: &HashMap<PlayerId, Entity>`; `capture/restore` | kancalar pencereyi kapatmamalı | — | spawn, restore | C (dikiş) |
| `game/kick.rs` | `kick(world, entity)` dünyaya `Resource` (`KitKicks`) yazar; `forward_kicks` okur | — | — | — | B (dünya mesaj kutusu olarak) |
| `game/lit.rs` (`LitGame`) | `light(world, viewer: Entity)`, `lit(.., world, record: Entity, wire)` | — | — | — | B |
| `common/mod.rs` | `on_leave`: despawn; `stamp_orphans`: `With<M>, Without<WireId>` sorgusu + `insert(WireId)` | — | `Orphans` (`Cached`) | despawn, damga | C |
| `common/cached.rs` | `QueryState` + `WorldId` + arketip nesli | — | **bevy'ye özgü optimizasyon** (A12); nesil değişince yeniden kurma yalnız bayt kimliği için | — | bağlayıcıya taşınır |
| `common/cells/book.rs` (`CellBook`) | `dirty_pass`: `(Entity, &WireId, Query)` + `(Dirty, With<Marker>)`; `sweep_removed`: `world.removed::<WireId>()` + `removed::<M>()`, `get_entity`, `contains`; `last_cell`, `members` `Entity` anahtarlı | **`Changed<T>`**, silinen tamponlar | `DirtyPass` (`Cached`) | çıkışları tamponlardan | D (+C) |
| `common/cells/book/index.rs` | `wire → Entity` (lit oda) | — | — | — | A (anahtar tipi) |
| `common/cells/book/rate.rs`, `cells.rs`, `cells/pieces.rs`, `pieces/assemble.rs` | — | — | parça önbelleği (ECS'siz) | — | **A** |
| `common/frame*`, `common/ledger*` (`SetLedger`, `Baselines`), `common/input*` (`InputSeq`, taşınan girdi) | — | — | — | — | **A** |
| `common/hooks.rs` | `join/join_with`: spawn kancası, `debug_assert!(contains::<Marker>)`, `insert(WireId)`; `ingest`, `systems`; `close_change_window` = `clear_trackers`; `guard_change_window` = `last_change_tick` karşılaştırması | **pencere sahibi** | — | spawn+damga | C+D |
| `common/park.rs` | `ParkEntry.entity: Entity`; `may_release(world, entity)` | — | — | süre dolunca despawn (oda üstünden) | C (anahtar) |
| `common/session.rs` | `session_private(world, entity, out)` | — | — | — | B |
| `room/` (`OpenRoom`) | her yayında tam kayıt geçişi (`RecordPass`), öksüz damgası | yok (son yükle karşılaştırır) | `RecordPass`, `Orphans` | damga | B |
| `aoi/` (`AoiRoom`) | `dirty_pass`, `sweep_removed`; `group_of` geri düşüşü: `world.entity(e).get_components::<Query>()` (tek varlık) | **D** | `DirtyPass`, `Orphans` | ayrılışta park edilen çıkış | D+C |
| `aoi/lit*` (`LitAoiRoom`) | ışık geçişi `LitGame` üzerinden dünyayı okur; `wire → Entity` indeksi | AOI'ninki | — | — | B |
| `team/` (`TeamRoom`) | `TeamMember`, `SightRadius` **kit'in bileşenleri**; `Sighted` tam geçiş (her tick yeniden kurma); katılımda `insert(TeamMember)` | yok (tam yeniden kurma + `SetLedger`) | `Sighted`, `Orphans` | bileşen yazar | B+C |
| `pvs/` (`SectorRoom`) | `Placed` tam geçiş; `group_of`: `world.entity(e).get::<M::Pos>()` | yok | `Placed`, `Orphans` | — | B |
| `space/` | `Vision::Pos`, `Partition::Pos`, `SectorMap::Pos` sınırı `Component`; geri kalanı saf | — | — | — | A (sınır kalkar) |
| `budget.rs` | — | — | — | — | A |
| `client/` (`ClientView`) | — | — | — | — | **A** |
| `sharded/mod.rs`, `mig.rs`, `room/policy.rs`, `crystal/{book,counters}.rs`, `team/budget.rs`, `spatial.rs` | — | — | — | — | A |
| `sharded/room*` (`ShardedRoom`) | `player_entity`, `wire_entity`, `entity_wire` tabloları; `removed::<WireId>()` süpürmesi; öksüz damgası (parça sayacı); `border`/`own`/`crossing` tam geçişleri (konum + kayıt); göç: `capture`/`restore`, despawn | silinen tampon | 4 `Cached` | göç, damga, despawn | B+C |
| `sharded/spatial*` (`ShardedSpatialRoom`) | AOI'nin `dirty_pass`/`sweep_removed`'ı + ödünç şerit | **D** | `DirtyPass` | — | D |
| `sharded/seam*` (`Seam`, alan sorguları) | `Seam::local(wire) -> Entity`, `SeamView::local(EntityRef)`, `Holder::Local(Entity)`, `Disabled` süzgeci | — | — | — | B+C |
| `sharded/departing.rs` | kesinleşmiş göçmen kopyalarını oyun kancalarından gizlemek için **`Disabled` ekle/çıkar** (bevy'nin varlık devre dışı bırakma özelliği) | — | — | **bevy'ye özgü özellik** | C |
| `sharded/crystal*` | sahip olunan tellerin konumu: `world.get::<P::Pos>(e)` | — | — | — | B |
| `sharded/team*` | `Sighted` tam geçiş; varışta `insert(TeamMember)`, `insert(SightRadius)` | — | `Sighted` | bileşen yazar | B+C |
| `testing/*` (fikstür oyun), `record_run/*` (kayıtlı oturum karşılaştırmaları), `budget/rooms.rs` | yalnız testler: bevy bileşenleri, `Changed<Position>`, `RoomFactory<World>` | — | — | — | test düzeneği |
| `manifest.rs` | (test) `gsb-ecs`'i motor crate'i sayar | — | — | — | — |

### 3.1 Envanterin söylediği

1. **Motor zaten ayrık.** Sıcak yolun pahalı kısmı (parça kodlama,
   paket birleştirme, roll, küme farkları) ECS'e hiç dokunmuyor. ECS
   dokunuşu her odada `update`'in başındaki **besleme** ile sınırlı:
   kayıtları topla, değişenleri/gidenleri bildir, pencereyi kapat.
2. **Yedi dikiş** (S1–S7):
   - **S1 kayıt kaynağı:** yayın kümesi (`Marker`), tel değeri
     (`Query` → `wire`), tam geçiş (`RecordPass`, `Placed`, `Sighted`,
     `border/own/crossing`) ve tek varlık okuması (`group_of`).
   - **S2 değişim kaynağı:** `Dirty` filtresi + kit'in pencere kapatması
     (`clear_trackers`) + pencere bekçisi (`last_change_tick`).
   - **S3 çıkış kaynağı:** `removed::<WireId>()`, `removed::<Marker>()`.
   - **S4 yaşam döngüsü:** oyun kancası spawn eder, kit `WireId`
     damgalar; öksüz damgası; despawn (ayrılış, park sonu, göç).
   - **S5 strateji ekleri:** `TeamMember`, `SightRadius`, görüş/sektör/
     bölge konumu (`Vision::Pos`, `SectorMap::Pos`, `Partition::Pos`),
     ışık (`LitGame`).
   - **S6 dünyada tutulan kit durumu:** `KitKicks` kaynağı, `Disabled`.
   - **S7 kanca imzaları:** `Game*` kancalarının `World`/`Entity` tipleri,
     `players: &HashMap<PlayerId, Entity>`, `Seam`/`SeamView`/`Holder`.
3. **Varlık bir anahtar olarak** her yerde (`last_cell`, `members`,
   `player_entity`, `wire_entity`, `ParkEntry.entity`, `hidden`): bunlar
   `Owner`'a dönüşünce A sınıfına geçer.

## 4. Değişim algılama ECS'lere nasıl eşlenir

| | bevy_ecs | hecs | shipyard (not) |
|---|---|---|---|
| Değişim | `Changed<T>`: bileşen başına değişim tick'i; `Mut<T>`'nin `DerefMut`'u işaretler (aynı değer yazımı da). Filtre **arketip düzeyinde değil**: eşleşen her varlıkta bir tick karşılaştırması — O(N) ama ucuz. Pencere: kit'in `clear_trackers` çağrısı. | **Yok.** Seçenekler: (a) `Tracked<T>` sarmalayıcı — `DerefMut` bir bayrak koyar (bevy'nin `Mut` kuralının aynısı, yapısal: her yazan işaretler), bağlayıcı bayrakları O(N) doğrusal geçişte okur+siler; (b) fark taraması — her kaydı her tick bildir, kit'in tel karşılaştırması değişmeyeni atar (O(N) hash araması); (c) `hecs::ChangeTracker<T>` — gizli `Previous<T>` bileşeni, `Clone + PartialEq`, O(N) kopyala-karşılaştır, despawn'ları **görmez**; (d) açık kirli listesi — disiplin, reddedildi (AOI modül dokümanı: "yazanlar unutmasın" üç kez ısırdı). | Bileşen deposu başına izleme (`track::Modification`/`All`): `modified()` / `inserted()` yineleyicileri, `clear_all_modified`; bevy'ye 1:1. |
| Çıkış | `world.removed::<T>()` tamponları (pencereyle birlikte döner). | **Yok.** (a) **günlüklü dünya** (`KitWorld`): `hecs::World` üstünde sarmalayıcı; yalnız `&World`'e deref eder (sorgular, `query::<&mut T>()` yazımları serbest), yapısal çağrılar (`despawn`, `remove_one`, …) sarmalayıcıdan geçer ve günlüğe yazılır — oyunun başka despawn yolu yok, yani yapısal; (b) **sayım** (`Census`): bağlayıcı taradığı kayıtları sayar, sayı defterin bildiğine eşitse hiçbir şey gitmemiştir (kararlı durumda maliyet sıfır), değilse tarama sırasında yazılan yoğun `indeks → (tick, bitler)` damgasında bir sıralı geçiş. | `deleted()` / `removed()` yineleyicileri. |
| Pencere sahibi | kit (`close` = `clear_trackers`) | kit (`close`: günlüğü siler; bayraklar okunurken silinir) | kit (`clear_all_*`) |
| Varlık ⇄ `Owner` | `Entity::to_bits()` / `from_bits` | `Entity::to_bits().get()` / `from_bits` | `EntityId::inner()` / `from_inner` |
| Göçmen gizleme (`Disabled`) | `Disabled` bileşeni (varsayılan sorgu süzgeci) | eşdeğeri yok → `World::take(e)` + kancalardan sonra `spawn_at(e, ..)` (aynı tanıtıcı) | eşdeğeri yok → aynı "al ve geri koy" ya da bir süzgeç bileşeni |

Spike (a)–(b)'yi hecs için gerçekledi ve ölçtü (§8): `Tracked<T>` +
`KitWorld` bevy ile aynı hızda; fark taraması 3–25 kat yavaş (yalnız
geri düşüş); sayım kayıt başına ~2.5 ns ekliyor (despawn olmayan tick'te
bile, damga yazımı yüzünden).

## 5. Önerilen mimari

### 5.1 Katmanlar

```text
gsb-core (ECS yok)          gsb-kit (ECS YOK)                          bağlayıcılar (ince)
GameLogic<W> …   ◄───────   motorlar: CellBook, CellPieces, SetLedger, ◄─── gsb-kit-bevy-ecs  (bevy_ecs)
Owner (Calls ile ortak?)    Baselines, InputSeq, park, frames, client;      gsb-kit-hecs      (hecs)
                            odalar: AoiRoom<G,S> …  → GameLogic<World<G>>   (gsb-kit-shipyard — ileride)
                            dikiş: Ecs, RecordFeed, strateji kaynakları
```

- **`gsb-kit` (işlevsel, ECS'siz):** bugünkü her motor + her oda. Odalar
  oyunun seçtiği ECS'e jenerik kalır: `impl<G: Game, S> GameLogic<World<G>>
  for AoiRoom<G, S>` (`World<G> = <G::Ecs as Ecs>::World`). Varlık
  kit'in içinde **`Owner`**'dır; bütün tablolar `Owner` anahtarlı.
- **`gsb-kit-bevy-ecs`:** `Bevy` işaretçi tipi (`impl Ecs for Bevy`),
  `KitWire(WireId)`, `TeamMember`, `SightRadius` bileşenleri (bevy
  derive), `Changed<T>` beslemesi, kalıcı `QueryState` (`Cached`),
  `Disabled` ile gizleme, `EntityExt`/`OwnerExt`, `GsbKicks` yeni tipi
  (`ResMut` ile), bugünkü `gsb-ecs`'in `System`/`SystemRunner`'ı (§5.7).
- **`gsb-kit-hecs`:** `Hecs` işaretçi tipi, `Tracked<T>`, `KitWorld`,
  `take`/`spawn_at` ile gizleme, aynı uzantılar.

Yetim kuralı yüzünden bağlayıcı `impl gsb_kit::Ecs for bevy_ecs::World`
yazamaz (ikisi de yabancı); **yerel bir işaretçi tipi** (`pub struct
Bevy;`) üstüne yazar — SERVICES §5.5'in `GsbCalls` yeni tip gerekçesiyle
aynı.

### 5.2 Bağlayıcı sözleşmesi (taslak)

```rust
// gsb-kit (ECS'siz)
pub trait Ecs: Send + 'static {
    type World: Send + 'static;
    type Entity: Copy + Eq + Hash + Debug + Send + 'static;
    fn owner(e: Self::Entity) -> Owner;
    fn entity(o: Owner) -> Option<Self::Entity>;
    fn alive(w: &Self::World, e: Self::Entity) -> bool;
    fn despawn(w: &mut Self::World, e: Self::Entity);
    fn hide(w: &mut Self::World, e: Self::Entity);   // departing (bevy: Disabled)
    fn show(w: &mut Self::World, e: Self::Entity);
}

/// S1–S3: bir oyunun kayıtlarını bir dünyadan okumak (spike'taki
/// `CellFeed`'in genellemesi). Kalıcı sorgu durumunu kendisi tutar.
pub trait RecordFeed<E: Ecs, W>: Default + Send + 'static {
    fn stamp_orphans(&mut self, w: &mut E::World, mint: impl FnMut() -> WireId);
    fn stamp(&mut self, w: &mut E::World, e: E::Entity, id: WireId);      // join
    fn changed(&mut self, w: &mut E::World, sink: impl FnMut(Owner, u64, W));
    fn all(&mut self, w: &mut E::World, sink: impl FnMut(Owner, u64, W)); // tam geçiş
    fn one(&self, w: &E::World, e: E::Entity) -> Option<(u64, W)>;         // group_of
    fn gone<C>(&mut self, w: &E::World, known: &HashMap<Owner, (u64, C)>,
               sink: impl FnMut(Gone));
    fn close(&mut self, w: &mut E::World);                                // pencere
}

pub trait RecordCodec: Send + 'static {   // ECS'siz kalan yarı
    type Wire: Clone + Eq + Debug + Send + 'static;
    const RUN: bool = false;
    fn encode(&self, id: u64, wire: &Self::Wire, out: &mut BytesMut);
    fn send_every(&self, wire: &Self::Wire) -> SendEvery { SendEvery::Tick }
}

pub trait Game: Send + 'static {
    type Ecs: Ecs;
    type Codec: RecordCodec;
    type Feed: RecordFeed<Self::Ecs, Wire<Self>>;
    fn spawn_player(&mut self, world: &mut World<Self>, conn: ConnectionId) -> Ent<Self>;
    fn ingest(&mut self, world: &mut World<Self>, ctx: &TickCtx,
              actions: &mut Vec<Action>, players: &Players<Self>, seq: &mut InputSeq);
    // … bugünkü kancalar, aynı adlar; World/Entity yerine ilişkili tipler
}
```

Bir bevy oyunu için imzalar normalleşir: `type Ecs = gsb_kit_bevy_ecs::Bevy;`
→ `fn spawn_player(&mut self, world: &mut bevy_ecs::world::World, conn) ->
bevy_ecs::entity::Entity` — oyun kodu bugünküyle aynı okunur. Kayıt
beslemesi bağlayıcının jenerik tipiyle seçilir (bugünkü `Marker`/`Query`/
`Dirty` üçlüsü oraya taşınır):

```rust
// bevy oyunu
type Feed = gsb_kit_bevy_ecs::Records<Position /* Marker */, &'static Position /* Query */,
                                      Changed<Position> /* Dirty */, ToCm /* wire */>;
// hecs oyunu
type Feed = gsb_kit_hecs::Records<Tracked<Position>, ToCm>;
```

### 5.3 Strateji ekleri (S5)

- Uzay trait'leri `Component` sınırını bırakır: `Vision::Pos: Copy`,
  `SectorMap::Pos`, `Partition::Pos` düz tipler. Sınır yalnız oradaydı.
- Takım odaları kayıt başına `(pos, team, sight)` ister: kit'te düz veri
  (`Team`, `f32`), bağlayıcıda bileşen (`TeamMember(Team)`,
  `SightRadius(f32)` — bevy derive'ı bağlayıcıda). `TeamFeed<E, W, P>`
  `all`'ın zenginleştirilmiş biçimidir; katılımda takımı yazmak
  `Ecs`-bağlayıcısının işi (`set_team(w, e, t)`).
- `LitGame`, `SeamView::local`, `Game::session_private`, `may_release`,
  `capture` gibi **oyunun kendi okumaları** oyunun ECS'iyle yazılır —
  kit yalnız `&World<G>` ve `Ent<G>` geçirir; kit'in kendisi okumaz.

### 5.4 `Owner`

- `Owner(u64)`, opak; kit onu hash'ler ve karşılaştırır, varlığa yalnız
  onu üreten bağlayıcı geri çevirir. bevy: `Entity::to_bits`
  (indeks+nesil), hecs: `Entity::to_bits`, shipyard: `EntityId::inner`,
  ECS'siz oyun: kendi kimliği.
- SERVICES §5.5'teki `Owner` ile **aynı tip** olmalı (açık soru K1):
  bir istek sahibinin varlığı ile bir kaydın sahibi aynı şeydir.

### 5.5 Belirlenim: kanonik sıra (YENİ)

Bugün bir hücrenin değişim listesinin sırası bevy sorgusunun sırasıdır
(arketip sırası), `Cached`'ın "arketip nesli değişince yeniden kur"
kuralı da tam bu sıranın taze bir sorguyla aynı kalması için var (A12).
İki ECS'in sorgu sırası farklı olduğundan, ECS'siz çekirdek baytları
sıradan bağımsız kılmalı: **çekirdek her dokunulan hücrenin değişim
listesini (ve tam parçalarını) tel kimliğine göre sıralar**
(`CellBook::canonicalize`). Maliyet O(m log m), m = tick'in değişimleri.
Sonuç: aynı senaryo bevy ve hecs üstünde bayt bayt aynı paketler (spike
testi); `Cached`'ın yeniden kurma kuralı yalnız bir bevy optimizasyonu
olarak kalır (ya da kalkar — K6). `record_run`'ın içerik pinleri çerçeve
içinde sıralı karşılaştırır (`layout.rs`), yani etkilenmez; tel sırası
bir kez değişir — istemci kuralları (`kit.proto`: kayıtlar mutlak,
idempotent upsert; sıra yalnız `removed → cell_exits → records`) buna
izin verir.

Öksüzler (oyunun join dışı spawn ettikleri) kimliğini damga geçişinin
sırasıyla alır, o da ECS'in sırasıdır: tek bir ECS içinde belirlenimli,
iki ECS arasında farklı (K11).

### 5.6 Dünyada tutulan kit durumu (S6)

- `KitKicks` (`Resource`): kit'in kendi kuyruğu olur; oyun `TickCtx`
  ya da kit'in verdiği bir tutamaç üstünden atar. bevy bağlayıcısı
  sistemler için `GsbKicks` yeni tipini `Resource` olarak sunar
  (`GsbCalls` ile aynı desen), oda fazlarında `resource_scope` ile
  boşaltır.
- `Disabled`: `Ecs::hide/show` arkasına (hecs: `take` + `spawn_at`).

### 5.7 `gsb-ecs`'in akıbeti

`gsb-ecs` = `bevy_ecs` + 60 satırlık `System`/`SystemRunner`. Tek
kullanıcısı `gsb-demo`; `gsb-server`'daki bağımlılık ölü. **Öneri:**
`gsb-kit-bevy-ecs`'in `systems` modülüne katılır, `gsb-ecs` silinir
(DESIGN §7 metni oraya işaret eder). hecs tarafında karşılığı gerekmez:
hecs sistemleri düz fonksiyondur (`fn sys(w: &mut World, ..)`).

### 5.8 `gsb-server`'da `bevy_ecs` isteğe bağlı

```toml
[dependencies]
bevy_ecs = { workspace = true, optional = true }   # yalnız oyun modülleri
# gsb-ecs: kaldırılır (kullanılmıyor)

[features]
game-demo  = ["dep:gsb-demo",  "dep:gsb-kit", "dep:bevy_ecs"]
game-arena = ["dep:gsb-demo-arena", "dep:gsb-kit", "dep:bevy_ecs"]   # hecs'e geçince: "dep:hecs"
game-mmo   = ["dep:gsb-demo-mmo", "dep:gsb-kit", "dep:bevy_ecs"]
game-war   = ["dep:gsb-demo-war", "dep:gsb-kit", "dep:bevy_ecs"]
```

Kapı: `cargo tree -p gsb-server --no-default-features -e normal -i bevy_ecs`
boş olmalı (bugün `gsb-kit`'in `manifest` testinin yaptığı türden bir
yapısal denetim olarak eklenebilir). Faz P3'ten sonra oyun modülleri
`bevy_ecs`'i doğrudan değil, bağlayıcı üstünden adlandırır.

### 5.9 Crate / özellik yerleşimi

| Crate | Bağımlılıklar | Not |
|---|---|---|
| `gsb-kit` | `gsb-core`, `gsb-protocol`, `prost`, `bytes`, `tracing` | **ECS yok** (özellik arkasında bile). |
| `gsb-kit-bevy-ecs` | `gsb-kit`, `bevy_ecs` | `Bevy`, `Records<..>`, bileşenler, `systems` (eski `gsb-ecs`). |
| `gsb-kit-hecs` | `gsb-kit`, `hecs` | `Hecs`, `Tracked<T>`, `KitWorld`, `Records<..>`. |
| `gsb-kit-conformance` (yayınlanmaz) | `gsb-kit`, iki bağlayıcı | Bugünkü `testing/` fikstürü + `record_run` + oda testleri, **her bağlayıcı için** (matris). |
| (isteğe bağlı) şemsiye `gsb` | yukarıdakiler | `bevy_ecs` / `hecs` özellikleri bağlayıcıları yeniden dışa aktarır; işlevsel kodda özellik YOK. |

İstek tarafının adaptörleri (SERVICES §5.5: `gsb-calls-bevy-ecs`,
`gsb-calls-hecs`) ile birleştirme: ECS başına **tek** bağlayıcı crate
(`gsb-bevy-ecs`: `kit` + `calls` modülleri) crate sayısını yarıya indirir
(K10). **q5'te (b) seçildi:** `gsb-bevy-ecs` ve `gsb-hecs` bugün yalnız
`calls` modülünü taşır; kit bağlayıcısı aynı crate'lere `kit` modülü
olarak girer (SERVICES §20 "Faz 5 — uygulanan biçim").

## 6. Hangi demo hecs'e taşınır

**Öneri: `gsb-demo-arena`.** En küçük demo (814 satır, 8 dosya), tek oda
(`TeamRoom`), `Game` + `TeamGame` yüzeyinin tamamını (spawn, takım,
`bot_actions`, `ingest`, `systems`, `session_private`) kullanır, 3B
görüş + `SendEvery` hız sınıfı içerir; sistemi tek bir hareket geçişi
(`QueryState<(&mut Pos3, &MoveTarget3, &Speed)>` → hecs'te
`query_mut::<(&mut Tracked<Pos3>, &MoveTarget3, &Speed)>()`). Taşıma
oyun kodunda tahminen ~100 satırlık bir değişiklik (sistem, spawn,
bileşen okumaları). Değişim algılamayı ise
**uyum matrisi** kanıtlar: kit'in kendi fikstür oyunu (`testing/`) her
odayı (AOI, sharded spatial dahil) iki bağlayıcıyla da koşar — bir
demonun yapamayacağı kadar geniş. `gsb-demo-mmo`/`-war` (sharded seam,
`Disabled`, göç) sonraya: önce `take`/`spawn_at` gizlemesi uyum
matrisinde kanıtlanmalı.

## 7. Göç planı (fazlar, risk)

| Faz | İş | Risk |
|---|---|---|
| **P0** (bu tur) | Tasarım + spike + ölçüm. | — |
| **P1** | `gsb-server`: `bevy_ecs` isteğe bağlı (oyun özellikleri), ölü `gsb-ecs` bağımlılığı kalkar; `cargo tree` kapısı. | Düşük (yalnız manifest). |
| **P2** | Kit içinde `Entity` anahtarları → `Owner` (bevy hâlâ içeride: `to_owner()` sınırda); `CellBook::observe`/`forget` ayrımı (dirty pass gövdesi ↔ sorgu); kanonik sıralama. Davranış aynı, tel sırası bir kez değişir. | Düşük-orta: `record_run` pinleri sıralı karşılaştırır; sıcak yol spike'ta ölçüldü (fark gürültü içinde). |
| **P3** | `RecordCodec` bölünür; `Ecs`/`RecordFeed`/strateji kaynakları; `Game` `type Ecs`/`type Feed` alır; `Cached`, `KitWire`, `TeamMember`, `SightRadius`, `Disabled`, `KitKicks` yeni `gsb-kit-bevy-ecs`'e taşınır; uzay trait'leri `Component` sınırını bırakır; `gsb-kit`'ten `bevy_ecs` düşer. Kit testleri dünyaya muhtaç olduğundan `gsb-kit-conformance`'a taşınır (kit → bağlayıcı geliştirme bağımlılığı bir döngü olur ve kit'in ikinci kopyasını derler — `gsb-kit/Cargo.toml`'daki uyarının aynısı). Demolar `type Ecs = Bevy` ile derlenir. | **Yüksek emek**, orta risk: ~48 dosya; en ince yer sharded seam (`SeamView::local(EntityRef)`, `Holder::Local(Entity)`) ve `departing` (`Disabled`). Mekanik ama geniş; oda oda yapılabilir (önce `CellBook`/AOI, sonra open/pvs/team, en son sharded). |
| **P4** | `gsb-kit-hecs` (`Tracked<T>`, `KitWorld`, `take`/`spawn_at`); uyum matrisi hecs'le yeşil; `gsb-demo-arena` hecs'e. | Orta: hecs'te gizleme ve öksüz damgası sırası yeni. |
| **P5** | `gsb-ecs` → `gsb-kit-bevy-ecs::systems`; demolar bağlayıcı üstünden; (isteğe bağlı) şemsiye özellikleri; istek adaptörleriyle birleşme (K10). | Düşük. |
| **P6** (isteğe bağlı) | shipyard bağlayıcısı (bir kullanıcı çıkarsa); yoğun `Owner` indeksi optimizasyonu (§9, K12). | Düşük. |

## 8. Prototip ve ölçüm (`spikes/k0-kit`)

### 8.1 Ne kanıtlandı

- `core/` (`k0-kit-core`, **hiçbir ECS bağımlılığı yok**): `Owner`,
  `WireId`/`Minter` (bileşen derive'ı olmadan), `Grid2`, `CellBook`
  (bugünkü `book.rs`'in dört ilkeli ve roll'u **kelimesi kelimesine**,
  `Entity` → `Owner`; `dirty_pass` → `observe` gövdesi, `sweep_removed` →
  `forget` gövdesi; hız ertelemesi ve lit indeksi ECS'e dokunmadığından
  dışarıda), `canonicalize`, paket birleştirme, `CellFeed` dikişi ve
  `run_tick` (`AoiRoom::update`'in sırası).
- `bevy-ecs/`: `KitWire`, `Cached` (A12, taşındı), `BevyCellFeed<T, M>`
  (`Changed<T>` + `With<M>`, silinen tamponlar, `clear_trackers`).
- `hecs/`: `Tracked<T>`, `HecsTrackedFeed` (bayraklar + sayım),
  `KitWorld` + `HecsLoggedFeed` (bayraklar + günlük), `HecsDiffFeed`
  (değişim kaynağı yok).
- `scenario/`: bugünkü bevy-yerli yolun **kopyası** (`native.rs` — kıyas
  tabanı), betikli dünya (oyuncu %10; tick başına hareket, alt-kuantum
  hareketler, NPC despawn/spawn, oyuncu ayrılış/katılış), üç eşitlik
  testi (`tests/equal.rs`): **beş sürücü her tick aynı kanonik defter
  durumunda** (kovalar, değişim listeleri, appeared/exited bayrakları,
  doğan gruplar, üye sayıları) ve **dört ECS'siz sürücü her üye
  hücresinin paketini bayt bayt aynı** üretiyor. Testler mutasyonla
  sınandı (hecs çıkış denetimi, bayrak okuması, `KitWorld` günlüğü
  bozulunca kırmızı).

### 8.2 Sayılar

Ölçülen: `AoiRoom::update`'in oyun sistemlerinden sonraki kısmı (dirty
pass / `changed`, park edilen çıkışlar, süpürme / `gone`, kanonik
sıralama — yalnız ECS'siz sürücülerde —, roll, pencere kapatma), tick
başına. `cargo run --release --bin bench` (`lto = "thin"`,
`codegen-units = 1`); her sürücü 5 tur, turlar araya serpiştirilmiş;
tablo = turların tick medyanlarının medyanı (µs). Senaryo: kayıtların
%10'u oyuncu, hücre başına ~10 kayıt, tick başına kayıtların `hareket`
%'si yazılır (1/8'i alt-kuantum), NPC'lerin ‰1'i despawn + o kadar spawn,
bir oyuncu ayrılır + bir oyuncu katılır.

**Makine yükü:** 32 çekirdek; ölçüm boyunca `uptime` yük ortalaması
**12–38** (paralel q1 turu ve diğer işler). Bu yüzden **koşular arası**
oynama büyük (aynı sürücü 10k/%10'da 221–381 µs); anlamlı olan **aynı
koşu içindeki** sürücüler arası karşılaştırmadır. Birincil tablo en düşük
yüklü koşu (3. koşu, yük 18–25); parantez içinde üç koşunun aralığı.

| kayıt | hareket | bugünkü bevy-yerli | çekirdek+bevy_ecs | çekirdek+hecs `Tracked`+sayım | çekirdek+hecs `Tracked`+`KitWorld` | çekirdek+hecs fark taraması |
|---|---|---|---|---|---|---|
| 1k | %10 | **16.3** (16.3–17.4) | **17.8** (17.8–18.1) | 19.6 (19.6–20.5) | **17.4** (17.4–19.4) | 70.7 (66.9–70.7) |
| 10k | %10 | **221** (221–381) | **209** (209–339) | 226 (226–356) | **196** (196–298) | 807 (807–1010) |
| 10k | %1 | **32.2** (32.2–86.9) | **32.5** (32.5–73.4) | 55.7 (55.7–115) | **30.4** (30.4–50.0) | 701 (701–876) |
| 100k | %10 | **12 171** (12 171–33 389) | **12 281** (12 281–31 587) | 17 508 (17 508–43 577) | **12 622** (12 622–39 496) | 78 372 (77 171–182 623) |

Despawn olmayan senaryo (`CHURN=0`, yük 12–25): 10k/%1 → 33.6 / 29.2 /
53.3 / 29.6 / 621; 100k/%10 → 14 853 / 13 843 / 14 814 / 14 258 /
69 365 µs (aynı sıra).

Oyun tarafı (bilgi için): yazımlar (`apply`) hecs'te
3–6 kat ucuz (`query_one_mut` ↔ bevy `get_mut`); kit'in işi değil.

### 8.3 Okuma

1. **Çekirdek + bevy_ecs = bugünkü yol.** Her boyutta aynı koşu içinde
   fark ±%10'un içinde ve yön koşudan koşuya değişiyor (10k'da üç
   koşunun ikisinde çekirdek daha hızlı). Tek tutarlı fark 1k'da ~+1 µs
   (sıralama + `gone` vektörü + `Owner` dönüşümü) — tick bütçesinin
   (33 ms) ~%0.003'ü. **Sıcak yol bağlayıcıya taşınınca yavaşlamıyor.**
2. **Çekirdek + hecs (`Tracked<T>` + `KitWorld`) = bevy.** hecs'in
   değişim algılaması olmamasına rağmen bayrak sarmalayıcısı bevy'nin
   `Changed<T>`'siyle aynı maliyette; günlüklü dünya çıkışları O(çıkış)
   bulur.
3. **Sayım** kayıt başına ~2.5 ns ekliyor (10k'da ~+23 µs; despawn
   olmayan senaryoda da — maliyet damga yazımında, sıralı geçişte değil;
   bu ayrıca ölçüldü: damgasız sürüm 29–39 µs). Varsayılan değil, ham
   `hecs::World` için geri düşüş (K3).
4. **Fark taraması** (değişim kaynağı yok) 3–25 kat yavaş: her kayıt her
   tick bir hash araması. Yalnız son çare.
5. **100k'da** dört varyant aynı mertebede ve süre hareket eden başına
   ~1.2 µs: defterin `HashMap`'leri (SipHash, rastgele erişim, önbellek
   ıskası) baskın, ECS değil (§9 madde 5, K12).

## 9. Performans riskleri ve sıcak yolun hızını korumak

1. **Dinamik dağıtım yok.** Besleme jenerik (`impl FnMut` geri çağrımı,
   monomorfize); spike'ta `run_tick` + `BevyCellFeed` bugünkü döngüyle
   aynı hızda (§8.2 — fark ölçüm gürültüsünün içinde). `dyn` yalnız sunucunun oda fabrikasında kalır (bugünkü gibi).
2. **Kanonik sıralama** O(m log m), m = tick'in değişimleri; tam parça
   sıralaması tam parça kodlandığında (doğan grup, keep-alive, tek seferlik
   özel tam) — parça önbelleği sayesinde hücre başına tick'te bir kez.
3. **hecs'te çıkış:** sayım kayıt başına ~2.5 ns damga yazar (despawn
   olmasa da). Varsayılan `KitWorld` günlüğü olmalı (O(çıkışlar)); sayım
   ham `hecs::World` kullanan oyun için geri düşüş.
4. **hecs'te değişim:** fark taraması O(N) hash araması — varsayılan
   değil. `Tracked<T>` bayrak taraması bevy'nin `Changed<T>`'si ile aynı
   biçimde (O(N) ucuz kontrol + O(hareket edenler) defter işi).
5. **Büyük dünyalar (100k):** maliyeti bağlayıcı değil, defterin
   `HashMap`'leri (SipHash, rastgele erişim, önbellek ıskaları) belirliyor
   — üç bevy/hecs varyantı aynı mertebede. İyileştirme bağlayıcıdan
   bağımsız: `Owner` yoğun bir indeks ipucu taşırsa (bevy
   `Entity::index`, hecs `Entity::id`) `last_cell`/`members` `Vec`
   tablolarına dönebilir; ayrıca `FxHash`/`ahash` gibi hızlı hasher
   (K12). Bugünkü kit'te de aynı fırsat var.
6. **Tam geçişli odalar** (open, pvs, team, sharded border/own/crossing)
   zaten her tick O(N); bağlayıcı kayıt başına bir satır içi geri çağrım
   ekler. P3'te her oda için kayıtlı oturum + ölçümle doğrulanmalı.
7. **`group_of` geri düşüşü** (`one`): yalnız katılım tick'inde, oyuncu
   başına bir okuma — değişmez.
8. **Bekçi (`guard_change_window`):** bevy'de `last_change_tick`
   karşılaştırması bağlayıcıya taşınır; hecs'te karşılığı gerekmez
   (pencereyi kapatan bir API yok).

## 10. Açık sorular (her biri öneriyle)

1. **K1 — `Owner` nerede?** (a) `gsb-core`'da, istek tasarımının
   `Calls`'ı ile ortak; (b) kit'in kendi tipi. **Öneri: (a)** — q1
   turunun `Calls` tablosunu getirdiği yerde tek tip; kit onu yeniden
   dışa aktarır. q1 ile koordinasyon gerekir (bu tur `gsb-core`'a
   dokunmadı).
2. **K2 — `Game`'in ECS'i nasıl seçilir?** (a) ilişkili tip `type Ecs`;
   (b) jenerik `Game<E: Ecs>`. **Öneri: (a)** — bir oyun bir ECS seçer;
   odalar `AoiRoom<G, S>` olarak kalır, imzalar normalleşince oyun kodu
   bugünküyle aynı okunur. (b) aynı oyunu iki ECS'te koşmayı sağlar ama
   her odaya bir tip parametresi ekler; buna ihtiyaç yalnız testlerde var
   ve onu uyum matrisi fikstür oyununu iki kez yazarak karşılar.
3. **K3 — hecs çıkış kaynağı:** (a) `KitWorld` günlüklü sarmalayıcı;
   (b) sayım; (c) açık despawn yardımcısı (disiplin). **Öneri: (a)
   varsayılan, (b) ham `hecs::World` için geri düşüş**; (c) reddedilir
   (yapısal değil).
4. **K4 — hecs değişim kaynağı:** **Öneri: `Tracked<T>`** (bevy'nin
   `Mut` kuralı; ölçüldü). Fark taraması yalnız geri düşüş; `hecs::ChangeTracker`
   despawn görmez ve her kaydı kopyalar — önerilmez.
5. **K5 — Kanonik sıra:** çekirdek değişim listelerini ve tam parçaları
   tel kimliğine göre sıralasın mı? **Öneri: evet** (ECS'ten bağımsız
   bayt; ölçülen maliyet gürültü içinde). Tel sırası bir kez değişir;
   istemci kuralları sıra varsaymıyor (kayıtlar mutlak upsert).
6. **K6 — `Cached`'ın arketip-nesli yeniden kurması:** kanonik sıradan
   sonra bayt kimliği için gereksiz; `QueryState` yeni arketipleri zaten
   artımlı öğrenir. **Öneri: P3'te kaldır** (bevy bağlayıcısında yalnız
   `WorldId` kontrolü kalsın), kayıtlı oturumla doğrula.
7. **K7 — `gsb-ecs`:** **Öneri: `gsb-kit-bevy-ecs::systems`'e katıl,
   crate'i sil**; `gsb-server`'daki ölü bağımlılık P1'de kalkar.
8. **K8 — Kit testleri nerede?** Kit ECS'siz olunca testlerinin dünyası
   bir bağlayıcıdan gelir; kit → bağlayıcı geliştirme bağımlılığı döngü
   kurar. **Öneri: `gsb-kit-conformance`** (yayınlanmaz): fikstür oyun +
   `record_run` + oda testleri her bağlayıcıyla. Saf motor testleri
   (`ledger`, `pieces`, `client`, `rate`, …) kit'te kalır.
9. **K9 — Göçmen gizleme hecs'te:** (a) `take` + `spawn_at` (aynı
   tanıtıcı, oyun kancaları görmez — yapısal); (b) bir `Hidden` bileşeni
   ve oyunun sorgularında `Without<Hidden>` (disiplin). **Öneri: (a).**
10. **K10 — Bağlayıcı crate sayısı:** (a) özellik × ECS
    (`gsb-kit-bevy-ecs`, `gsb-calls-bevy-ecs`, …); (b) ECS başına tek
    (`gsb-bevy-ecs` = `kit` + `calls` modülleri). **Öneri: (b)** — bir
    oyun ECS'ini bir kez seçer; şemsiye `gsb` crate'inin `bevy_ecs`
    özelliği bu tek crate'i yeniden dışa aktarır. (Kullanıcı
    `gsb-kit-bevy-ecs` adını örnek verdi; ad kararı kullanıcının.)
11. **K11 — Öksüz kimliklerinin sırası** ECS'in sorgu sırasıdır (tek
    ECS içinde belirlenimli, ECS'ler arası farklı). **Öneri: kabul** —
    REPLAY tek ECS içinde çalışır; ECS'ler arası bayt eşitliği yalnız
    join-yolu kimlikleri için garanti edilir (spike testi bunu sınar).
12. **K12 — Yoğun `Owner` indeksi / hızlı hasher:** büyük dünyalarda
    defterin `HashMap`'leri baskın (§8). **Öneri: P2'den sonra ayrı bir
    ölçüm turu**; bağlayıcı kararından bağımsız.
13. **K13 — Strateji ekleri nasıl beslenir?** (a) strateji başına kaynak
    trait'i, bileşenler bağlayıcıda (`TeamMember`, `SightRadius`); (b)
    oyun her kayıt için düz bir "birim" yapısı döndürür. **Öneri: (a)**
    — bugünkü "takım dünyada bir bileşendir" kararı (team modül
    dokümanı, madde D) korunur: çalışma anında takım değişikliği yine
    düz bir bileşen yazımıdır.
14. **K14 — shipyard:** **Öneri: bağlayıcı yazılmasın** (kullanıcı
    çıkana kadar); sözleşme ona da uyar (§4 tablosu: izleme bevy'ye
    1:1).
