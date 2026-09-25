# Oyun Modülü — Sunucunun Herhangi Bir Oyunu Barındırması (Tasarım)

**Durum: ONAYLI İŞ SIRASININ 4. MADDESİ (2026-09-25) — uygulama fazları
aşağıda.** Kararlar §6. Girdisi, `gsb-server`'ın `gsb-demo`'ya bağlılığının
salt-okuma haritasıdır (dosya:satır referanslı; özet §2).

## 1. Neden

`gsb-kit` ayrımından sonra üç örnek oyun var (`gsb-demo` 2D,
`gsb-demo-arena` 3D takım sisi, `gsb-demo-mmo` 3D grid + shard). Motor
çekirdeği ve kit oyundan bağımsız; ama **kompozisyon kökü** `gsb-server`
(config, boot, dinleyiciler, ops HTTP, `gsb-server` ikilisi) ve
`gsb-loadgen` hâlâ 2D demo'ya sabit. Sonuç: arena ve MMO yalnız crate
testleriyle doğrulanabiliyor — gerçek istemcilerle uçtan uca
çalıştırılamıyor, yük altında ölçülemiyor. Bu doküman sunucunun **kit
üzerine kurulmuş herhangi bir oyunu** barındırmasını ve loadgen'in
herhangi bir oyunu sürebilmesini tasarlar.

## 2. Bugünkü bağlılık (harita özeti)

- Tek bağımlılık satırı `gsb-server/Cargo.toml:31` (`gsb-demo`); kit
  tiplerinin hepsi demo'nun yeniden ihracı üzerinden geliyor.
- **Fabrikalar** (`boot/factories.rs`): altı oda tipi, demo oyunuyla ve
  demo ayarlarıyla (`spawn_half`, `cell_size`, `vision_radius`,
  ekonomi servisi) kuruluyor.
- **Başlatma** (`boot/start.rs:204-339`): altı kollu `match`; her kol
  kendi `Registry<W, G, St, Sp>` tipini başlatıyor, `Registry::new`'in 7
  argümanı altı kez tekrarlanıyor.
- **Config:** düz `Config` struct'ında demo-özel alanlar (`topology`,
  `visibility`, `communication`, `shard_count`, `aoi_cell_size`,
  `team_vision_radius`, `spawn_half_size`, `disconnect_grace_secs`) ve
  üç-eksen çözümleyici (`resolve_selection`, `RoomKind`, eksen hata
  varyantları).
- **Join yönlendirme:** `home_shard = shard_at(spawn_pos(conn, …))` —
  demo'nun spawn formülünü tekrar ediyor.
- **Mesaj tablosu:** `build_table()` = base + `gsb_demo::register`.
- **Loadgen:** demo'nun `MoveTo`'sunu kodluyor, `WorldSnapshot`/`Private`
  aynasını çözüyor, `Grid2` hücre hesabını ve demo'nun spawn kafesini
  kopyalıyor.

**Zaten oyundan bağımsız olanlar (dokunulmaz):** dinleyiciler + TLS, kabul
hattı (yalnız `Arc<MessageTable>` ister), ops HTTP (yalnız registry
mailbox'ı + metrik `watch`), `ServerHandle` ve durdurma sırası, bilet
kancası, ticker, metrik toplayıcı, oda ön-oluşturma, pump zaman aşımları;
loadgen'de çerçeveleme, taşıma istemcisi, auth/join/leave, GSM8 metrik
formatı, rapor katlama, orkestratör.

## 3. Temel gözlem: tip silme registry'nin başlatıldığı yerde

`RegistryMsg` generic değildir (`gsb-core/src/registry/msg.rs:18`).
Başlatılmış registry görevinin dışındaki her şey yalnızca bir
`Mailbox<RegistryMsg>` tutar; `W`, `G`, `St`, `Sp` generic'leri yalnızca
`start.rs`'in her kolunun **içinde** yaşar. Bu yüzden oyun modülü
**registry'yi kendisi başlatırsa**, modül trait'inin hiçbir generic
parametresi olmaz ve `Arc<dyn GameModule>` olarak tutulabilir. Config ve
boot'a hiçbir tip sızmaz; altı kollu `match` tek bir çağrıya iner;
`gsb-core`'a dokunulmaz.

> KIT-ARCHITECTURE §4 "tip silme fabrikada yapılır" diyordu; bu kesin
> değildi. Fabrikanın imzası `G`, `St`, `Sp`'yi `Registry<W, G, St, Sp>`'ye
> taşır. Gerçek generic-olmayan sınır `Mailbox<RegistryMsg>`'dir.

## 4. Tasarım

### 4.1 `GameModule` trait'i (gsb-server kütüphanesinde)

```rust
/// Sunucunun bir oyunu barındırmak için bilmesi gereken her şey.
/// Nesne-güvenli: generic parametre yok.
pub trait GameModule: Send + Sync + 'static {
    /// `game = "..."` config anahtarındaki ad; RESULT satırına da yazılır.
    fn name(&self) -> &'static str;
    /// Oyunun ayarlarını ham config tablosundan okur ve doğrular.
    /// Desteklemediği bir anahtar AÇIKÇA yazılmışsa başlatmayı reddeder.
    fn configure(&mut self, raw: &toml::Table, engine: &Config) -> Result<(), ServerError>;
    /// Oyunun mesajlarını base tabloya ekler.
    fn register(&self, table: &mut MessageTable);
    /// Registry'yi (ve oyunun servislerini) başlatır. Generic'ler
    /// yalnızca bu metodun içinde yaşar.
    fn spawn_registry(&self, parts: RegistryParts) -> JoinHandle<()>;
    /// RESULT/başlangıç satırı için seçimin tek satırlık tarifi.
    fn describe(&self) -> String;
}

pub struct RegistryParts { /* inbox, self_mailbox, ticker, metrics,
    max_connections, max_unauth, result_sink */ }
impl RegistryParts {
    /// Yöntem-düzeyi generic: bir modül kendi fabrikasıyla çağırır.
    pub fn spawn<W, G, St, Sp>(self, factory: RoomFactory<W, G, St, Sp>) -> JoinHandle<()>;
}
```

`start_server(cfg)` imzası korunur ve varsayılan modülü (2D demo)
kullanır; yeni bir `start_server_with(module, cfg)` giriş noktası eklenir.

> **G1'de uygulanan biçim** bu taslaktan dört noktada ayrılır (ad
> çakışması, `RegistryTask`, `Config::raw`, `Box`); gerekçeleriyle
> §5 "G1 sonucu"nda.

### 4.2 Modüllerin yeri: A + B melezi

- `GameModule`, `RegistryParts` ve `run(module)` `gsb-server`
  kütüphanesindedir.
- Repodaki üç oyunun adaptörleri `gsb-server/src/games/{demo,arena,mmo}.rs`
  içinde, her biri **isteğe bağlı bir cargo özelliğinin** arkasında
  (varsayılan: hepsi açık). Oyun çalışma zamanında seçilir.
- Üçüncü taraf bir oyun `default-features = false` ile bağımlanır, trait'i
  kendi crate'inde uygular ve kendi `main`'inden `run(module)` çağırır.
- **Yapısal oyun-bağımsızlık kanıtı:** `cargo build -p gsb-server --lib
  --no-default-features` hiçbir oyun olmadan derlenmelidir; bu bir CI
  kapısı olur.
- Kit, üç demo ve `gsb-core` bu işte **değişmez** (kit'in kabul
  disiplini korunur). Değişmesi gerekirse tasarım bulgusu olarak raporlanır.

### 4.3 Config bölünmesi

Motor kendi anahtarlarını bugünkü gibi düz `Config` üzerinden okur. Modül
ham `toml::Table`'ı alır:

- **2D demo** eski düz anahtarlarını okumaya devam eder (geriye uyumluluk;
  mevcut config'ler ve testler değişmez). `Config`'in demo alanları bu
  turda **uyumluluk için kalır** (struct-literal kullanan testler
  kırılmaz); çözümleyici, `RoomKind` ve eksen hataları demo modülüne taşınır.
- **Arena ve MMO** ayarlarını **kendi adlarını taşıyan** bir tablodan
  okur (`[arena]`, `[mmo]`) — bkz. §6 karar 1'in G1 düzeltmesi.
- Bir oyunun sabitlediği bir şeyi (arena için eksenler; MMO için
  `aoi_cell_size`, `shard_count`) ayarlayan **açıkça yazılmış** bir anahtar
  başlatmada hata verir — sessizce yok sayılmaz. Açık yazılmış olup
  olmadığı ham tablodan anlaşılır (`Config` `#[serde(default)]` olduğu için
  struct'tan anlaşılamaz).

### 4.4 Loadgen: `LoadBot` + ortak istemci görünümü

- Oyun başına bir `LoadBot` (dyn): bir bot için bir sonraki girdiyi, flood
  girdisini ve churn girdisini kodlar; `snapshot_op`/`private_op`'u,
  bir kayıt çözücüyü (`id, cell`) ve bir çıkış-hücresi çözücüyü verir.
- Loadgen'in içerik kontrolleri (bayat atma, baseline'sız düşürme,
  delta'nın removed → cell_exits → upserts sırasıyla uygulanması,
  tek-seferlik private full, private delta = hata) **kit'in istemci
  kurallarıdır** ve bugün üç kopyadır (loadgen `view.rs`, demo
  `delta_aoi.rs`, MMO test istemcisi). Bu kurallar tek bir generic
  `ClientView<Cell>`'e toplanır. *(G4'te `gsb_kit::client::ClientView<D:
  ClientDecoder>` olarak kuruldu — kopya sayısı altı çıktı; §5 "G4
  sonucu".)*
- **Demo botu bugünkü kodun birebir taşınmasıdır** (profiller, `as i32`
  kırpma, seq numaralandırma), yani girdileri bayt-bayt aynı kalır.
  *(G3'te uygulanan biçim — `LoadBot` + istemci başına `BotClient` —
  ve bayt kanıtı: §5 "G3 sonucu".)*

### 4.5 Değişmemesi gerekenler (2D demo)

Wire baytları; varsayılan config'in etkisi (OpenRoom + ekonomi, spawn
yarı-boyutu 50, grace 30 sn); eski eksen türetmesi ve hata varyantları +
mesajları; `start_server*` imzaları ve `lib.rs`'deki `pub use` listesi;
`gsb-server` paketindeki ikili adları (`gsb-server`, `gsb-loadgen`); her
loadgen bayrağı ve varsayılanı; CLIENT satır formatı; GSM8 metrik formatı;
mevcut her RESULT anahtarı ve değeri. **Tek RESULT değişikliği:** satırın
sonuna eklenen yeni bir `game=<ad>` anahtarı.

## 5. Fazlar

| Faz | Kapsam | Kapı |
|---|---|---|
| G1 ✅ | Trait + `RegistryParts`; `factories.rs` + çözümleyici demo modülüne **olduğu gibi** taşınır; `Config` alanları ve `resolve_selection` uyumluluk katmanı olarak kalır; `--no-default-features` derlemesi; §6 karar 10'daki orkestratör düzeltmesi | tüm testler değişmeden yeşil; loadgen A/B gürültü içinde |
| G2 ✅ | Arena ve MMO modülleri (config bölümleri, MMO join yönlendirici, politika eşlemesi) + ikisinin gerçek `Registry` üzerinden uçtan uca testleri | yeni e2e testleri; kit/demo/core diff'i boş |
| G3 ✅ | Loadgen: `LoadBot` + generic görünüm, demo botu birebir, `--game` her iki çocuğa iletilir, arena ve MMO botları, ilk ölçüm tabanları | demo için RESULT/CLIENT birebir (+`game=`); arena/MMO ilk sayılar |
| G4 ✅ | Kit istemci kurallarının `gsb_kit::client` modülüne alınması (tetikleyici var: üç kopya) + çözücü seam'i; demo ve MMO test istemcilerinin ona geçirilmesi — **G3'ten önce koşuldu** (ebeveyn kararı) | test iddiaları değişmez |

### G1 sonucu (2026-09-25)

**Tamam.** Beş kod commit'i (+ bu doküman), her biri kendi başına yeşil: iki hata düzeltmesi
(önce kırılan testleriyle), seam + demo modülü, özellik kapısı, RESULT
anahtarı. `gsb-core`, `gsb-kit` ve üç demo crate'i değişmedi. Test
sayısı 521 → 535 (+2 orkestratör, +6 ekonomi, +5 modül, +1 varsayılan
kilidi; 1 ignored doctest aynı); hiçbir mevcut iddia değişmedi —
yalnız `loadgen_smoke`'a `game=demo`nun varlığı ve SON anahtar olduğu
iddiası EKLENDİ. Loadgen A/B (base `e96fc94` ↔ HEAD, dönüşümlü, dört
senaryo × iki tur): her RESULT anahtarı aynı sırada ve aynı biçimde,
sayılar gürültü içinde, satır sonunda yeni `game=demo`.

**Son imzalar** (`gsb-server/src/game.rs`):

```rust
pub trait GameModule: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn configure(&mut self, raw: &toml::Table, engine: &Config) -> Result<(), ServerError>;
    fn register(&self, table: &mut MessageTable);
    fn spawn_registry(&self, parts: RegistryParts) -> RegistryTask;
    fn describe(&self) -> String;
}
pub struct RegistryParts { /* alanlar crate-içi: inbox, self_mailbox,
    ticker, metrics, max_connections, max_unauth_conns, result_sink */ }
impl RegistryParts {
    pub fn spawn<W, G, St, Sp>(self, factory: RoomFactory<W, G, St, Sp>) -> RegistryTask
    where W: Send + 'static, G: Eq + Hash + Clone + Debug + Send + 'static,
          St: Debug + Send + 'static, Sp: Debug + Clone + PartialEq + Send + 'static;
}
pub struct RegistryTask(/* JoinHandle<()>, özel */);
pub enum GameError { Unknown { name, compiled_in }, NameMismatch { configured, module },
                     Module { game, source } }
// ServerError::Game(#[from] GameError) — #[error(transparent)]
pub async fn start_game_server(module: Box<dyn GameModule>, cfg: Config) -> …;
pub async fn start_game_server_with(module, cfg, hooks, report_tx: Option<…>) -> …;
```

Yerleşim: `src/game.rs` (seam), `src/games.rs` (katalog: `compiled_in()`,
`by_name()`, `DEFAULT_GAME`), `src/games/demo.rs` (+ çocuklar `axes.rs`,
`select.rs`, `factories.rs` — sonuncusu `git mv`), `src/boot/start.rs`
(`start_inner` artık `module.configure` → `module.register` →
`module.spawn_registry(parts)`), `src/boot/start/entry.rs` (altı giriş
noktası).

**§4 taslağından sapmalar ve nedenleri:**

1. **Giriş noktasının adı.** `start_server_with(cfg, hooks)` zaten
   public API (e2e testi kullanıyor) — `start_server_with(module, cfg)`
   onu kırardı. Yeni çift: `start_game_server(module, cfg)` ve
   `start_game_server_with(module, cfg, hooks, report_tx)`. Mevcut
   `start_server*` ailesi imza olarak aynı; oyunu `cfg.game`'e göre
   katalogdan seçer. Açık modülle başlatılıp config DOSYASI başka bir
   `game` yazıyorsa `GameError::NameMismatch` (karar 2'nin ruhu: sessiz
   yok sayma yok).
2. **`spawn_registry` → `RegistryTask`** (`JoinHandle<()>` değil).
   `RegistryTask`'ı yalnız `RegistryParts::spawn` üretebilir, yani bir
   modül registry'yi başlatmayı "unutamaz" — tip düzeyinde garanti.
3. **Ham tablo = `Config::raw`** (`#[serde(skip)] pub raw: toml::Table`),
   `Config::from_file` doldurur; kodla kurulan config'te boştur. Neden:
   `start_server(cfg)` yalnız `Config` alır; ham tablonun taşınacağı
   başka yer yok. Sonuç (G2 için not): açık-yazılmış anahtar tespiti
   yalnız DOSYADAN gelen config'te çalışır; kodla kurulan config (testler,
   loadgen) hiçbir anahtarı "açık" saymaz.
4. **`configure(&mut self)`**, modül `Box<dyn GameModule>` olarak
   `start_inner`'a taşınır (Arc değil): başlatmadan sonra kimse modülü
   paylaşmıyor, fabrika kapanışları ihtiyaç duyduklarını kopyalıyor.
   Yapılandırılmamış modülün `spawn_registry`'si bir `expect` ile
   panikler — sırayı sunucu garanti eder (configure bind'dan önce,
   hata varsa hiçbir şey başlamaz).
5. **Eksen hata varyantları `ServerError`'da kaldı** (§6.12 "demo
   modülüne taşınır" yerine). Mevcut testler ve public API
   `ServerError::ShardedCrossInterest` vb. üzerinden eşleşiyor; mesajlar
   bayt-bayt aynı. Taşınan, ÜRETİCİ oldu (çözümleyici + shard-sayısı
   kontrolü demo modülünde). `RoomKind`, `VisibilityAxis`,
   `ResolvedSelection` demo modülüne taşındı ve kök yollarında yeniden
   ihraç ediliyor; `Visibility`, `Topology`, `Communication` `Config`
   alan tipleri oldukları için config'te kaldı.
6. **`run(module)` (§4.2) G1'de eklenmedi.** `gsb-server` ikilisinin
   `main`'i (config okuma + sinyal bekleme) olduğu gibi; üçüncü taraf
   bugün `start_game_server` + kendi sinyal beklemesiyle çalışır. Arena
   ve MMO (G2) aynı ikiliden `game` anahtarıyla seçileceği için G2 de
   buna ihtiyaç duymuyor; ayrı küçük bir iş.
7. **`Config`'in demo varsayılanları sunucu-yerel sabitler**
   (`DEMO_DEFAULT_VISION_RADIUS` = 25, `…_SPAWN_HALF` = 50,
   `…_DISCONNECT_GRACE_SECS` = 30) — `Config` oyunsuz derlensin diye;
   `games/demo/tests.rs` her birini `gsb-demo`'nun kendi sabitine kilitler.
8. **Oyunsuz derleme:** `gsb-demo` isteğe bağlı bağımlılık, varsayılan
   açık `game-demo` özelliği. `--no-default-features` ağacında `gsb-demo`
   ve `gsb-kit` yok. `gsb-server` ikilisi oyunsuz da derlenir ve
   başlangıçta `Unknown { name: "demo", compiled_in: [] }` ile reddeder;
   `gsb-loadgen` ve `examples/client.rs` demo protokolünü konuştuğu için
   `required-features = ["game-demo"]`. Entegrasyon testleri `gsb_demo`
   kullanır, oyunsuz test koşusu kapı değildir. CI: `no-game` işi
   (`cargo build -p gsb-server --lib --no-default-features` + aynı
   özelliklerle lib+bins clippy).
9. **`describe()`** yalnız başlangıç log satırında kullanılıyor
   (`game module configured`); RESULT'a yalnız `name()` giriyor.

**G2 için bulgular:**

- **TOML anahtar çakışması.** §6.3 `game = "arena"` (dize) ile §4.3
  `[game]` tablosu aynı TOML belgesinde birlikte OLAMAZ (aynı anahtar
  iki kez tanımlanır — ayrıştırma hatası). G2 seçmeli: oyun adıyla
  anılan tablo (`[arena]`, `[mmo]`) ya da `[game]` içinde `name = …`.
  Öneri: `[arena]`/`[mmo]` — `game` dizesi bugünkü gibi kalır, bir
  config birden çok oyunun bölümünü taşıyabilir, modül yalnız kendi
  tablosuna bakar.
- **Demo'nun AOI/team/PVS kurucu trait'lerinde `with_economy` yok**
  (`OpenRoomExt` ve iki sharded trait'te var). Karar 11'in düzeltmesi
  `gsb-demo`'ya dokunmadan kit'in public `game_mut()`'u + demo'nun
  `set_economy`'si ile yapıldı; demo'ya simetrik `with_economy`
  eklemek küçük bir temizlik olarak kalıyor.
- `boot/start.rs` 296 satır (hedefin üstünde): tek sürekli başlatma
  prosedürü; giriş noktaları `start/entry.rs` çocuğuna alındı.

**Hata düzeltmeleri:** karar 10 (orkestratör istemci çocuklarına
`--cell-size` iletmiyordu; istemci komut satırı saf bir fonksiyona
alındı, iki test önce `None` ile kırıldı) ve karar 11 (ekonomi servisi
altı yapının hepsine bağlı; `tests/economy_rooms.rs` altı yapının her
birinden gerçek bir `ECONOMY` gidiş-dönüşü sürer — düzeltmeden önce
aoi/team/sector "economy service not configured" ile kırıldı).

En riskli parça G3: loadgen'in sıcak döngüsünü (`run.rs:111-445`) demo
için bayt ve sayaç bazında aynı tutarak yeniden yazmak. En büyük bilinmeyen
G2: MMO'nun gerçek `Registry` altında **ilk kez** çalışması — kaydı
olmayan oturumların yönlendirilmesi, churn modunda shard'lar arası resume,
`room_count > 1` ya da `/rooms/open` ile birden fazla MMO dünyası. Bu, F1
gibi yeni bulgular çıkarabilir; çıkarırsa raporlanır.

### G2 sonucu (2026-09-25)

**Tamam.** Altı commit (+ bu doküman), her biri kendi başına yeşil:
arena modülü, MMO modülü, ortak test desteği + arena e2e + config
dosyası testleri, MMO e2e'leri, iki kit bulgusunun kilidi, örnek config.
`gsb-core`, `gsb-kit`, `gsb-demo`, `gsb-demo-arena`, `gsb-demo-mmo`
**değişmedi** (`git diff 27f2b1e.. --stat` bu beşinde boş). Test sayısı
551 → 579 (+10 birim: ayar okuma / reddetme / yönlendirici; +18
entegrasyon: 2 arena e2e, 4 config dosyası, 3 MMO e2e, 3 çıkış/resume,
1 çoklu dünya, 2 bulgu kilidi, 3 örnek config); 1 ignored doctest aynı;
hiçbir mevcut test değişmedi. Özellikler: `default = ["game-demo",
"game-arena", "game-mmo"]`; `--no-default-features` ve her oyun
özelliği tek başına (`--no-default-features --features game-arena` /
`game-mmo`) lib + bins clippy temiz.

**Arena (`games/arena.rs`, `game = "arena"`).** Tek strateji: tek oda ×
takım sisi × always-full — `TeamRoom<ArenaGame, VisionGrid3<Pos3>>`,
her oda kimliğine yeni bir `ArenaGame` (round-robin her odada baştan).
`[arena]` tablosu:

| Anahtar | Varsayılan | Anlamı |
|---|---|---|
| `teams` | 3 (`DEFAULT_TEAMS`) | oda başına takım, 1..=255 |
| `disconnect_grace_secs` | 30 (kit'in `DEFAULT_DISCONNECT_GRACE`) | düşen oyuncunun birimi bu kadar bekletilir, sonra arenanın botu onu üssüne götürür (AI devri); 0 = hemen çıkar |

Görüş yarıçapı (15 m, 3D) oyunun sabiti; operatöre açılmadı.

**MMO (`games/mmo.rs`, `game = "mmo"`).** Her oda kimliği BÜTÜN bir
shard'lı dünya: MMO'nun dört `ShardedSpatialRoom` shard'ı tek realm'den.
Yönlendirme (§6 karar 6; K4'ten beri kayıt oyuncunun doğrulanmış
kimliğiyle anahtarlı — aşağıda): kayıtlı karakteri olan oturum kaydının
shard'ına (`world::home_shard`), olmayan **varsayılan durak taşının
(0) shard'ına** — o shard'ın `spawn_player`'ı kaydı olmayan karakteri
tam oraya koyuyor; birim testi ikisini birbirine sabitliyor. Politika
(§6 karar 5): MMO'nun çıkış sayacı + savaş vetosu. `[mmo]` tablosu:

| Anahtar | Varsayılan | Anlamı |
|---|---|---|
| `logout_grace_secs` | 20 (`LOGOUT_GRACE`) | düşen karakter bu kadar dünyada kalır (resume edilebilir), sonra çıkar — savaştaysa savaş soğuyana dek bekler (çekirdeğin `max_detach_hold`'u sınırlar); 0 = hemen çıkar (park yok, veto da yok) |
| `logout` | `"release"` | `"release"` = slot bırakılır (`ExpireTo::Despawn`); `"bot"` = çıkış botu en yakın durak taşına yürütür, slot tutulur (`AiHandover`) |

Realm (içerik + kayıtlı karakterler) TOML'a açılmadı: katalog
`Realm::standard()` kullanır; gömen taraf `MmoModule::with_realm(realm)`
+ `start_game_server` ile kendi realm'ini verir (testler böyle).

**İki oyunun da reddettikleri** (açıkça yazılırsa başlatma hatası,
mesaj anahtarı ve nedenini adlandırır — `games::settings::SettingsError`,
`GameError::Module`'ün kaynağı): üç eksen (`visibility`, `topology`,
`communication`), `shard_count`, `aoi_cell_size`, `team_vision_radius`,
`spawn_half_size` ve demo'nun düz `disconnect_grace_secs`'i.

**§4 taslağından sapmalar ve nedenleri:**

1. **Demo'nun düz `disconnect_grace_secs`'i her iki oyunda da
   REDDEDİLİYOR, eşlenmiyor.** §6.5 "reddedebilir" diyordu; seçilen bu.
   Arena kendi grace'ini `[arena]`'dan okur (anlamı aynı: bekletme →
   bot). MMO'da anlam FARKLI: demo anahtarı "bekletme, sonra bot" demek,
   MMO'nunki "bekletme (savaşta uzar), sonra çıkış" — sessizce eşlemek
   oyun değiştiren bir operatörü yanıltırdı. Ret mesajı doğru anahtarı
   adlandırıyor (`[mmo] logout_grace_secs`).
2. **Oyunun kendi tablosunda bilinmeyen anahtar da hata** (taslakta
   yoktu; karar 2'nin ruhu — yazım hatası sessizce yok sayılmaz). Sabit
   anahtar tablonun İÇİNE yazılırsa da (`[mmo] shard_count`) aynı
   "sabit" hatası; tablonun kendi anahtarı bir düz anahtarla aynı adı
   taşıyabilir (arenanın `disconnect_grace_secs`'i) — bilinen önce gelir.
   Başka oyunun tablosu yok sayılır (bir dosya birkaç oyunun bölümünü
   taşıyabilir; demo da `[arena]`/`[mmo]`'ya bakmaz).
3. **`games::settings` public** (`own_table`, `integer`, `seconds`,
   `choice`, `SettingsError`): üçüncü taraf bir modül kendi tablosunu
   aynı kurallarla okuyabilsin.
4. **`gsb-kit` isteğe bağlı doğrudan bağımlılık** (iki oyun özelliği
   açar): fabrika tipleri kit'in `Team`, `Cell`, `KitMig`'ini adlandırıyor.
   Oyunsuz ağaçta hâlâ kit yok.
5. **`MmoModule::with_realm`** (taslakta yoktu): içerik ve kayıtlı
   karakterler koddan verilir; testlerin senaryolu realm'leri bunun
   üzerinden.
6. **Test desteği `tests/hosted/`**: TCP ya da TLS üzerinde çerçeveli
   istemci (okuyucu yarısı kendi görevinde, bounded kanal), oyun başına
   `View` (arena: full; MMO: kit'in istemci kuralları). MMO görünümü kit
   istemci kurallarının **dördüncü kopyası** — G4'ün tetikleyicisi
   güçlendi.

**Uçtan uca testler (gerçek `start_server`/`start_game_server`, gerçek
soketler) ve mutation-check'ler** (yedekten geri yüklenerek; korunan
crate'lere yapılan geçici mutasyonlar geri alındı, diff boş):

| Test | Kanıtladığı | Mutation → sonuç |
|---|---|---|
| `arena_e2e::three_teams_see_their_fog_over_{tcp,tls}` | `game = "arena"`, üç takım; alınan HER snapshot'ta sis kuralı ağ tarafından (düşman yalnız 15 m içinde); yükseklik: 20 m yukarıdaki gizli, 10 m'deki ikisini de görüyor; görüşe girip çıkma; numaralı girdiler ack'leniyor | arenada `VISION_RADIUS` 25 → kırıldı |
| `hosted_config::an_unknown_game_lists_all_three` | bilinmeyen oyun hatası `demo`, `arena`, `mmo`'yu listeliyor | — |
| `hosted_config::explicitly_written_fixed_keys_refuse_startup` | dosyadan her sabit anahtar reddediliyor, anahtarı adlandırıyor; `[arena] team` bilinmeyen | `visibility` sabit listesinden çıkarıldı → kırıldı |
| `hosted_config::the_arena_table_is_read_from_a_file` | `[arena] teams = 2` → üçüncü giren takım arkadaşını görüyor | `teams` yok sayıldı → kırıldı |
| `hosted_config::a_demo_config_without_a_game_key_still_hosts_the_demo` | `game`'siz eski config demo'yu başlatıyor; yanındaki `[mmo]` yok sayılıyor | — |
| `mmo_e2e::joins_land_on_the_shard_of_their_character` | kaydı olan shard'ına, olmayan shard 0'a (waystone 0) — basan shard wire id'nin aralığından okunuyor: göç değil, yönlendirme | yönlendirici hep varsayılan → kırıldı |
| `mmo_e2e::a_walker_across_a_seam_keeps_itself_and_is_seen_across_it` | x = 0 dikişini geçen oyuncu HER uygulanan karede kendini tutuyor, iki komşu da onu; geçişten sonra shard 0'daki komşu onu sınırın öbür yanından görüyor | kit'te F1 düzeltmesi geçici geri alındı → kırıldı ("lost") |
| `mmo_e2e::a_travel_lands_on_the_destination_shard` | `Travel` köşegen shard'a aynı wire id ile iniyor, oradaki oyuncu görüyor, sonraki girdiyi hedef shard işliyor ve ack'liyor | — |
| `mmo_logout::a_fighter_is_held_past_the_grace_and_a_peaceful_player_logs_out` | 1 sn çıkış: barışçıl olan grace sonrası çıkıyor, vuruş yapan savaşta TUTULUYOR, soğuyunca çıkıyor; registry iki satırı da bırakıyor | `release` → `AiHandover` → kırıldı; MMO'da veto kapatıldı → kırıldı |
| `mmo_logout::the_mmo_table_sets_the_logout_timer` | `game = "mmo"` + `[mmo] logout_grace_secs = 1` DOSYADAN, katalog üzerinden | grace yok sayıldı (20 sn) → kırıldı |
| `mmo_logout::a_character_parked_on_another_shard_resumes_there` | shard 0'da girip shard 1'e yürüyen, orada düşen karakter aynı adla resume ediliyor (registry'nin resume yayını buluyor), aynı wire id, girdi çalışıyor | — |
| `mmo_rooms::every_mmo_room_is_a_whole_sharded_world` | `room_count = 2` + `POST /rooms/open` ile açılan üçüncü oda: üç ayrı dünya (aynı waystone, üç id uzayında aynı ilk wire id, birbirini görmüyor, girdi yalnız kendi dünyasını oynatıyor) | — |
| `example_config::*` | örnek config demo'yu olduğu gibi barındırıyor; yorumdaki `[arena]`/`[mmo]` açılınca oyunları kabul ediyor; bulgu K5'in kilidi (*K5 turunda çevrildi — aşağıda "Kit düzeltme turu"*) | örnekte `logout = "despawn"` → kırıldı |

**`room_count > 1` ve `/rooms/open` ile birden fazla MMO dünyası
ÇALIŞIYOR** — beklenen risk çıkmadı: her oda kimliği fabrikadan bütün
bir shard grubu alıyor, registry grupları oda bazında tutuyor. Cross-
shard resume ve savaş vetosu da gerçek registry altında ilk denemede
doğru.

**Bulgular:**

- **K1 — Göçü tetikleyen girdi hiç ack'lenmiyor** (kit; test
  `mmo_findings::k1_…`). Kaynak shard oturumu MIGRATE'te (faz 4)
  devrediyor — ack'i yayacak BROADCAST'tan (faz 6) önce; hedef shard'ın
  `InputSeq`'inde oyuncu için iz yok. Bir `Travel`'ın ack'i hiç gelmiyor;
  bir SONRAKİ girdinin ack'i (yüksek su işareti) onu da kapsıyor.
  2D demo'nun sharded odaları aynı kit yolunda.
- **K2 — Sıra kuralı göçte sıfırlanıyor** (kit; test
  `mmo_findings::k2_…`). Shard içinde geç kalan numaralı girdi düşüyor;
  göçten sonra hedefte, `Travel`'dan KÜÇÜK numaralı bir girdi işleniyor
  — yeniden sıralanan ya da çiftlenen bir datagram (rUDP oyun bandı)
  tekrar oynatılır.
- **K3 — Kaynak shard'da `InputSeq` girdisi sızıyor** (kit; kod
  okuması, ağdan gözlenemiyor): `ShardedRoom::on_migrate_out`
  (`gsb-kit/src/sharded/room/shard.rs`) oyuncunun `input` kaydını
  `end` etmiyor; her göç kaynakta bir `states` girdisi bırakıyor (uzun
  ömürlü sunucuda göç eden oturum sayısıyla büyür). Çekirdek aynı yerde
  idle saatini (`idle.stop`) temizliyor; kit temizlemiyor.
  **K1–K3'ün en küçük düzeltmesi (gsb-kit, çekirdeğe dokunmadan):**
  `KitMig`'e oyuncunun işaretini ve bekleyen ack'ini (`hwm`, `acked`)
  eklemek; `collect_migrations` onu alıp kaynakta `input.end(player)`
  yapar, `on_migrate_in` hedefte kurar — bekleyen ack hedefin ilk
  yayınında çıkar (K1), kural sürer (K2), kaynak temizlenir (K3). O
  gün `mmo_findings` bilerek kırılır ve çevrilir.
  **K1–K3 çözüldü — `4d83d01`** (aşağıda "Kit düzeltme turu"; tek
  sapma: `collect_migrations` kaydı ALMIYOR, OKUYOR — `input.end`
  `on_migrate_out`'ta).
- **K4 — Kayıtlı karakterler oturuma bağlı; gerçek sunucu onları
  dolduramaz** (bilinen sınır, §6 karar 6 / KIT-ARCHITECTURE Faz 4
  gözlem 1; artık uçtan uca görünür). Bağlantı kimlikleri kabulde
  basılıyor, realm başlangıçta fabrikaya gömülü: katalogla barındırılan
  MMO'da HER oturum kaydısız → hepsi shard 0'a, waystone 0'a. Sonuç
  G3 için önemli: MMO botları yalnız yürüyüp `Travel` etmezse yük tek
  shard'da toplanır. Düzeltme çekirdekte (`on_join`'a hesap kimliği) ya
  da yönlendiriciye kimlik vermekte — bu işin kapsamı dışında.
  **Çözüldü — K4 turu, `888730a`..`ae0e680`** (aşağıda "K4 — oyuncu
  kimliği → ev shard'ı": yönlendirici ve join kancası doğrulanmış
  kimliği alıyor, realm onunla anahtarlı).
- **K5 — Örnek config oyun değiştirmeye tuzak** (test
  `example_config::switching_…`). `config.example.toml` demo'nun düz
  anahtarlarını (`visibility`, `aoi_cell_size`, `team_vision_radius`,
  `spawn_half_size`, `disconnect_grace_secs`) varsayılan değerleriyle
  AÇIKÇA yazıyor; kopyada yalnız `game = "arena"` yapmak başlatmayı
  `visibility`'de reddettiriyor (tasarım gereği doğru davranış).
  Örneğe not eklendi; kalıcı çözüm o satırları örnekte yoruma almak
  (demo için etkisi yok: değerler varsayılan) — bu turda yalnız ekleme
  yapıldı, karar bakımcıya. **Çözüldü — `80da518`** (aşağıda).

**Açık küçük iş:** CI'ın `no-game` işi yalnız `--no-default-features`'ı
derliyor; her oyun özelliğinin tek başına derlendiği iki adım
(`--no-default-features --features game-arena` / `game-mmo`) eklenmeli
(yerelde temiz). **Kapandı — `c8fa79a`** (aşağıda).

### Kit düzeltme turu (K1–K3, K5, CI) sonucu (2026-09-25)

**Tamam.** Üç commit (+ bu belge), her biri kendi başına yeşil; önce
kırılan testler, mutation-check'ler yedekten geri yüklenerek. `gsb-core`
**değişmedi** (`git diff 1b19ccb.. --stat -- crates/gsb-core` boş); wire
baytları değişmedi (`gsb-demo`'nun `wire_contract.rs` / `kit_wire.rs` /
`delta_aoi.rs`'i, kit'in `aoi/tests/sharing*`'i, arenanın ve MMO'nun
wire testleri dokunulmadan yeşil). Test sayısı 579 → 586 (+7 kit birim
testi); 1 ignored doctest aynı. Mevcut testlerde değişen yalnız: kit'in
üç testindeki `KitMig { … }` literal'ine `input: None` (derlenmesi için;
iddialar aynı), çevrilen iki kilit (`mmo_findings`, `example_config`)
ve `mmo_e2e`'de bir yorum.

| # | Commit | Değişiklik | Kanıtlayan test (önce kırıldı) | Mutation → sonuç |
|---|---|---|---|---|
| K1–K3 | `4d83d01` | `KitMig`'e `input: Option<ShardInputRecord { hwm, acked }>` (park kaydının yanında; `KitMig` ve kayıtları çocuk modül `sharded/mig.rs`'e taşındı). `collect_migrations` oyuncunun kaydını **okur** (almaz: reddedilen gönderimde oyuncu kaynakta kalır, çekirdek satırı geri alır ve sonraki tick yeniden toplar); `on_migrate_out` taşıma kesinleşince `input.end` (K3); `on_migrate_in` kaydı kurar (`InputSeq::adopt`) — hedefin sonraki private karesi kaynağın borçlu kaldığı ack'i taşır (K1), kural işaretini korur (K2). Kayıtsız bir varış yeni oturum başlatır. Spatial kompozit üç kancayı da delege ediyor: iki sharded oda da düzeldi | `gsb-kit` `sharded/tests/input_carry.rs` (7, fikstür oyunu): K1 (varışta ack 5, bir kez), zaten ack'lenmiş girdi yeniden ack'lenmiyor, K2 (4 ve 5 düşüyor, 6 işleniyor), K3 (commit'e dek kayıt kaynakta, sonra yok), reddedilen gönderim (kaynakta kural sürüyor, yeniden deneme aynı işareti taşıyor), köşegen iki adım (ara shard kurup devrediyor ve unutuyor), spatial kompozit (ack tek kez — ilk kare one-shot full ise bir tick sonra; K2, K3) — yedisi de düzeltmesiz kırıldı | hiç taşımamak → 6/7 kırıldı (+ `mmo_findings` ikisi de); `on_migrate_out`'ta `end` yok → K3, köşegen, spatial kırıldı; `collect`'te alıp silmek → reddedilen gönderim ve K3 kırıldı; `acked = hwm` ile kurmak → K1, reddedilen gönderim, köşegen, spatial kırıldı; `acked = 0` ile kurmak → "yeniden ack'lenmiyor" kırıldı |
| K5 | `80da518` | `config.example.toml`'da demo'nun beş düz anahtarı (`visibility`, `aoi_cell_size`, `team_vision_radius`, `spawn_half_size`, `disconnect_grace_secs`) yoruma alındı, varsayılan değerleriyle belge olarak kaldı; çevresindeki notlar güncellendi | `example_config.rs` çevrildi (3): örnek demo'yu barındırıyor, beş anahtarı yazmıyor ve anahtarlar açılmış hâliyle (yani eski örnekle) AYNI oda seçimine ve demo ayarlarına çözülüyor; yalnız `game` satırı değiştirilmiş kopya demo / arena / mmo'yu başlatıyor; yorumdaki tablolar geçerli (açma artık yalnız tablo bölümünde — düz `disconnect_grace_secs` de yorumda). Eski örneğe karşı üçü de kırıldı | yorumdaki `aoi_cell_size` 25 → kırıldı; `spawn_half_size` yeniden yazıldı → üçü kırıldı; yorumdaki `visibility` "spatial" → kırıldı |
| CI | `c8fa79a` | `no-game` işine `--no-default-features --features game-arena` ve `game-mmo` için build + clippy (`-D warnings`) adımları; işin adı aynı (zorunlu durum denetimi olabilir) | YAML PyYAML ile doğrulandı; dört komut yerelde temiz | — |

**`mmo_findings.rs`'te değişenler** (K1/K2 kilidi çevrildi): modül
belgesi düzeltmeyi anlatıyor; `k1_…_is_not_acked` →
`k1_the_input_that_moves_a_player_to_another_shard_is_acked` — iniş
sonrası 1,5 sn "ack yok" beklemesi yerine `acks == [1]` (Travel'ın ack'i
geliyor), sonraki girdiyle `acks == [1, 2]` (tek kez, sırayla);
`k2_…_restarts_…` → `k2_the_input_sequence_rule_holds_across_a_migration`
— shard 0 kısmı aynı; shard 3'te eski seq 2 artık 1 sn boyunca tanık
tarafından (256, 256)'da kalıyor (düştü), yeni seq 4 yürütüyor ve
ack'leniyor. `mmo_e2e`'de yalnız bir yorum ("Travel'ın ack'i
kayboluyor") güncellendi.

**Göç mesajının boyutu:** `Option<ShardInputRecord>` 24 bayt —
`KitMig<DemoMig>` 80 → 104, `KitMig<MmoMig>` 112 → 136;
`ShardMsg<KitMig<DemoMig>, StripPos>` 112 → 136, MMO'nunki 144 → 168
(`size_of`, 1b19ccb ve HEAD'de ölçüldü). `PlayerMigration` zaten
kutulu; clippy (`result_large_err` dâhil) temiz. **Codec:** bugün
`KitMig`'i hiçbir şey serileştirmiyor (in-process link taşıyor);
DISTRIBUTED §4b'nin Ipc/Net linki kit'in codec'ine bu alanı da katmalı
(`KitMig` belgesinde not).

**Loadgen A/B** (50 istemci, 4 shard, 8 sn; `1b19ccb` ↔ HEAD, dönüşümlü
üçer çift): sharded — `left=50 errors=0 server_closes=0` hepsinde,
`snap_total` 11468–11517 ↔ 11464–11533, `out_bps_per_conn` 7065–7141 ↔
7063–7108, `step_p50_fine_us` 24–56 ↔ 32–48; sharded × spatial — aynı
temiz sayaçlar, `snap_total` 11472–11494 ↔ 11488–11496,
`out_bps_per_conn` 2877–2893 ↔ 2894–2917, `step_p50_fine_us` 48–72 ↔
40–64. Gürültü içinde. Düzeltmenin yan izi: istemcinin saydığı ack
sayısı `acks` 2291–2296 → 2297–2298 (2298–2300 hamlede) — göçte
kaybolan ack'ler geri geldi.

### G4 sonucu (2026-09-25)

**Tamam — G3'ten ÖNCE** (ebeveyn kararı; §6 karar 9'un düzeltmesi):
G3'ün botları loadgen-yerel bir generic görünüm büyütmek yerine
doğrudan `gsb_kit::client`'ı kullanacak. Sekiz commit (+ bu belge),
her biri kendi başına yeşil:

| Commit | Değişiklik |
|---|---|
| `d0c88d2` | **Bulgu G4-1'in düzeltmesi** (aşağıda): loadgen ve `delta_aoi.rs` `CellExit`'i hücre İNDİSİ olarak okur; yeni test önce kırıldı |
| `513a419` | `gsb_kit::client` (görünüm, seam, zarf yürüyücüsü) + 17 kit testi |
| `2809a87` | loadgen görünümü kit'e geçti (`game-demo` özelliği artık `gsb-kit`'i açar) |
| `e652e82` | dört test istemcisi kit'e geçti (`delta_aoi.rs`, `aoi.rs`, MMO'nun `tests/common/client.rs`'i, `gsb-server/tests/hosted/mmo.rs`) |
| `b4c0039` | `examples/client.rs` kit'e geçti |
| `3a77e9b`, `4667c26`, `cfd7d3e` | performans (aşağıda "Alıcı döngü"): tembel hücre, `Copy` hata tipi, kayıtları ara belleğe almadan iki geçiş, satır içi yürüyücü |

`gsb-core` diff'i boş; wire baytları değişmedi (bayt sabitleyen testler
— `wire_contract.rs`, `kit_wire.rs`, kit'in `aoi/tests/sharing*`'i,
arena/MMO wire testleri — dokunulmadan yeşil). Test sayısı 586 → 609
(+20 kit, +2 loadgen çözücüsü, +1 `delta_aoi`); 1 ignored doctest aynı.

**Public API** (`gsb_kit::client`):

```rust
pub trait ClientDecoder {
    type Record;                 // görünümün entity başına tuttuğu
    type Cell: PartialEq;        // oyunun hücresi (CellExit'in adlandırdığı)
    fn record(&self, body: &[u8]) -> Result<(u64, Self::Record), ClientError>;
    fn cell_of(&self, record: &Self::Record) -> Self::Cell;   // sunucunun formülü
    fn cell_exit(&self, body: &[u8]) -> Result<Self::Cell, ClientError>;
}
pub struct ClientView<D: ClientDecoder>;   // new(decoder), Default (D: Default)
impl<D> ClientView<D> {
    pub fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError>;
    pub fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError>;
    // get(id), contains(id), len(), is_empty(), iter(), ids(), values(),
    // has_baseline(), last_sequence(), counters(), decoder()
}
pub struct Snapshot { pub sequence: u64, pub apply: Apply }
pub enum Apply { Full, Delta, NoBaseline, Stale }
pub enum PrivateEvent { Ack(u64), Full { sequence: u64 }, Empty }
pub struct Counters { pub fulls, pub private_fulls, pub deltas,
                      pub gap_drops, pub stale, pub errors }   // u64
pub enum ClientError { Malformed(&'static str), Body(prost::DecodeError), PrivateDelta }
// impl From<prost::DecodeError> for ClientError — tipli ayna `decode(body)?`
pub mod wire { pub struct Fields; pub enum Value; pub struct Malformed; pub fn sint32 }
```

`fulls` grup full'larını VE one-shot private full'ları sayar
(`private_fulls` alt kümesi) — loadgen'in ve `delta_aoi`'nin eskiden
saydığı gibi. Görünüm girdiyi ham bayt olarak alır (oyunun tipli
aynasını değil): zarf yerinde yürünür, kayıt ve hücre gövdeleri
çerçevenin alt dilimleri olarak seam'e gider.

**Seçilen bayat kuralı:** "son KABUL EDİLMİŞ sequence'tan `<=` olan
atılır; ilk kabulden önce hiçbir şey bayat değildir" (MMO istemcisinin
kuralı). Gerekçe `kit.proto`'nun kendi cümlesi: *"a duplicate (sequence
<= the last accepted) is discarded"* — kabul edilmiş kare yokken
karşılaştırılacak bir "son kabul" da yoktur; loadgen'in
`unwrap_or(0)`'ı olmayan bir kabul (0) uyduruyordu ve taze bir
istemcinin `sequence = 0` taşıyan full'unu (proto3 varsayılanı; kit'in
terminal `match_result` kare biçimi) bayat sayardı. Canlı akışta fark
gözlenemez (canlı sequence'lar tick'tir, kesin pozitif —
`room/logic.rs`), A/B'de de gözlenmedi; kit testi
`nothing_is_stale_before_the_first_accepted_frame` kuralı kilitliyor
(eski kural → kırıldı).

**§4.4 taslağından sapmalar:**

1. **`ClientView<Cell>` değil `ClientView<D: ClientDecoder>`**; seam
   üç metot: kayıt → `(id, Record)`, `cell_of(&Record)`, çıkış → hücre.
   Hücre kayıt başına değil, yalnız çıkış taşıyan bir delta servis
   edilirken, tutulan kayıt başına türetilir (ilk kesimde kayıt başına
   iki `f32` bölme + `floor` alıcı döngüyü yavaşlatıyordu).
2. **Zarf yürüyücüsü public** (`client::wire`): bir oyunun çözücüsü
   küçük bir kaydı üretilmiş tipin mesaj başına kurulumunu ödemeden
   elle yürüyebilir; loadgen'in demo çözücüsü böyle yapıyor (üretilmiş
   `EntityRecord` çözücüsüne iki testle sabitli). Test istemcileri
   tipli aynayla (`decode(body)?`) çözüyor.
3. **Hata anlamı:** bozuk bir zarf ya da hücre çıkışı görünümü HİÇ
   değiştirmez (ilk geçiş tüm zarfı doğrular); oyunun çözücüsünün
   reddettiği bir kayıt gövdesi ikinci geçişte, görünüm değişirken
   bulunur → görünüm BOŞ ve baseline'SIZ kalır (asla yarım kare),
   delta'lar bir sonraki full'a kadar düşer — taze istemci gibi.
   Eskiden loadgen böyle bir kareyi bütünüyle yok sayardı; canlı akış
   böyle bir gövde taşımıyor (her koşuda `errors=0`).
4. **`Private.responses`** görünümün parçası değil, atlanır; onu
   kullanan istemci kareyi tipli aynasıyla da çözer. **`Private.game`**
   (oyunun oturum yükü) de görünümün parçası değil: küçük paketten beri
   (G3-3) çözücünün `ClientDecoder::session_private`'ına verilir
   (KIT-ARCHITECTURE §5.1 "Oturum yükü").

**Bulgu G4-1 — demo tarafındaki iki kopya `CellExit`'i yanlış
okuyordu** (düzeltildi, `d0c88d2`). Kit'in `Grid2`'si çıkışı hücrenin
İNDİSİ olarak yazar (`game.proto`: "Cell index"); loadgen'in ve
`delta_aoi.rs`'in görünümü onu KONUM→hücre formülünden geçiriyordu
(`CellExit(1,0)` → `floor(1/20) = 0` → hücre (0,0)). Yalnız indisi
kendine eşlenen hücrelerin — (0,0), (−1,−1) gibi — çıkışı doğru
servis ediliyordu (`delta_aoi`'nin mevcut testi (0,0) kullandığı için
yakalamadı); diğer her çıkış yanlış hücrenin kayıtlarını unutturup
çıkan hücreninkileri bir sonraki keep-alive full'a kadar hayalet
bırakıyordu. MMO'nun iki test istemcisi zaten indis karşılaştırıyordu.
Yeni test `cell_exit_names_the_cell_index_away_from_the_origin`: beş
hareketli (1,0)'dan çıkarken gözlemci (0,0)'da, görünüm çıkış karesinin
KENDİ tick'inde denetleniyor (keep-alive onarmadan önce) — eski okumayla
"O never loses itself" kırıldı. **Etki ölçüldü** (geçici fark sondası,
her karede eski-kural görünümü ↔ kit görünümü): sayaçlar (`fulls`,
`deltas`, `gap_drops`, `private_fulls`) hiç değişmiyor; tutulan görünüm
50 istemci spatial'da karelerin %3–4,5'inden, sharded × spatial'da
%8–9'undan, 1000 istemci spatial'da %7'sinden sonra farklıydı; son
`view_size` yalnız koşu bir yanlış çıkışla sonraki keep-alive arasında
biterse farklı (spread 200: 7 ve 3 hayalet). Yani RESULT'taki
`view_size` base ↔ HEAD arasında, sistematik değil, bu nedenle
kayabilir.

**Taşınan kopyalar ve iddiaların değişmediğinin kanıtı:**

| Kopya | Ne oldu | Kanıt |
|---|---|---|
| loadgen `client/view.rs` (+ `view/run.rs`) | `ClientView = gsb_kit::client::ClientView<DemoDecoder>`; döngü ham yükü verir; sayaçlar görünümden | A/B + fark sondası (aşağıda): her karede eski görünümle (düzeltmeli) 0 görünüm farkı, 0 sayaç farkı |
| `gsb-demo/tests/delta_aoi.rs` | `View = ClientView<DemoDecoder>`; `Conn`'un sayaçları görünümden (`fulls()` …); ham çıkış karesi tek-kayıt iddiası için hâlâ tutuluyor | 29 `assert*` + önceki `assert!(!s.delta, "a private snapshot must be a full")` aynı mesajlı `panic!`; her iddia koşulu ve mesajı aynı, yalnız erişim sözdizimi (`.deltas` → `.deltas()`, `view.entities.get(&id)` → `view.get(id)`) |
| `gsb-demo/tests/aoi.rs` | **beşinci, kısmi kopya** (G1'de sayılmamıştı): `cell_exits`'i yok sayıyor, private snapshot'ı modu ne olursa olsun uyguluyordu; artık kuralların tamamı | 13 `assert*` aynı; test yeşil |
| `gsb-demo-mmo/tests/common/client.rs` | `pub view: ClientView<MmoDecoder>`; kimlik sıralı erişimciler sıralıyor (`sees`, `of_kind`) | yinelenen-entity denetimi görünümün uyguladığı HER karede (tipli aynayla) sürüyor; `assert!(!s.delta, …)` aynı mesajlı `panic!`; bütün MMO testleri değişmeden yeşil |
| `gsb-server/tests/hosted/mmo.rs` | G2'nin dördüncü kopyası; `records: ClientView<MmoDecoder>` | `book()` hâlâ her uygulanan karede (grup full/delta, private full); `mmo_e2e`/`mmo_logout`/`mmo_rooms`/`mmo_findings` değişmeden yeşil |
| `gsb-server/examples/client.rs` | **altıncı kopya** (loadgen'in `unwrap_or(0)` kuralıyla); aynı satırları sonuç başına basıyor | elle koşuldu: FULL/delta/ack/private satırları |

Dokunulmayan: `gsb-demo/src/demo/rooms/tests/sharded.rs`'teki
`ClientView` (gsb-demo `src`'sinde bir birim testi; tek akışı sıra
kuralı olmadan yeniden kuruyor — istemci kurallarının kopyası değil,
shard dikişinin wire denetimi).

**Loadgen A/B** (`646dd41` base ↔ `d0c88d2` fix ↔ HEAD, dönüşümlü üç
tur; `50 --duration 3`, `50 --duration 3 --visibility spatial`,
`50 --topology sharded --visibility spatial --shard-count 4 --duration
8`; `GSB_LOADGEN_CLIENT_LINES=1`): CLIENT ve RESULT satırlarının
anahtar kümesi, sırası ve biçimi üç ikilide de aynı; her koşuda her
istemci için `snapshots = fulls − private_fulls + deltas + gap_drops`
(bayat 0) ve CLIENT toplamları = RESULT; `errors=0`.

| Senaryo | `snap_total` | `fulls` | `private_fulls` | `deltas` | `gap_drops` | `view_size` |
|---|---|---|---|---|---|---|
| tcp (all) base / fix / HEAD | 4103–4150 / 4100 / 4101–4150 | = snap | 0 | 0 | 0 | 2500 hepsinde |
| spatial base / fix / HEAD | 4077–4134 / 4078–4138 / 4088–4140 | 235–286 / 227–284 / 239–289 | 78–83 / 71–79 / 81–84 | 3921–3933 / 3922–3933 / 3930–3935 | 0 | 1216–1264 / 1178–1250 / 1156–1176 |
| sharded × spatial base / fix / HEAD | 11458–11542 / 11445–11536 / 11456–11512 | 703–730 / 700–740 / 685–737 | 204–237 / 220–241 / 217–257 | 10992–11031 / 10965–11020 / 10978–11030 | 0 | 1528–1648 / 1571–1617 / 1559–1621 |

Sayaçlar gürültü içinde (`fulls` sunucunun taze-grup/keep-alive
kararından gelir; turdan tura ~50 oynar). **Belirleyici kanıt fark
sondası:** HEAD'e geçici olarak eklenen, her kareyi hem kit görünümüne
hem eski loadgen görünümüne (düzeltmeli ve düzeltmesiz) uygulayan bir
sonda; yedi koşuda (tcp, spatial ×2, sharded × spatial ×2, spread 200,
spatial 1000 — 1000 istemcide 123 921 kare) kit ↔ eski (düzeltmeli):
**0 görünüm farkı, 0 sayaç farkı**; kit ↔ base: 0 sayaç farkı, görünüm
farkı yalnız G4-1'den.

**Alıcı döngü (performans).** İlk kesim (`513a419`) sentetik tek
görünümde hızlıydı ama gerçek koşuda yavaştı; üç neden bulundu ve
giderildi: (a) kayıt başına hücre hesabı → tembel `cell_of`
(`3a77e9b`); (b) sıcak döngüdeki `Result<_, ClientError>`'ın düşürme
yapıştırıcısı (kutulu `DecodeError`) → yürüyücüde `Copy` `Malformed`
(`3a77e9b`); (c) görünüm başına kayıt ara belleği — binlerce görünümde
önbellekte sürekli ıskalanan bellek; eski tipli çözüm her karede
ayırıcıdan sıcak bellek alıyordu → kayıtlar ara belleğe alınmadan
ikinci geçişte doğrudan görünüme (`4667c26`); ve loadgen derlemesinde
yürüyücünün küçük fonksiyonları satır içine alınmıyordu →
`#[inline(always)]` + ikinci geçiş yalnız kayıt aralığını yürür
(`cfd7d3e`). Son ölçümler (kit / eski, kare başına):

| Ölçüm | spatial 1000 | sharded × spatial 50 | full-only (all 50 / 500) | spread 300 (1 kayıtlık kareler) |
|---|---|---|---|---|
| Yakalanmış gerçek kare akışlarının tekrarı (100 dönüşümlü istemci, 7 turun medyanı) | 0,72 | 0,69 | 0,60 / 0,70 | 0,72 |
| Canlı loadgen istemcisinde AYNI kareye iki görünüm (sıra dönüşümlü, süreç içi) | 0,76–0,79 | 0,63–0,69 | — / 0,76–0,78 | 0,97–1,17 (±50 ns / ~500 ns) |

`client_in_bps` A/B'de aynı (sunucunun gönderdiği). Orkestre modun
`clients_cpu_s`'i (1000 istemci, 2 süreç, spatial, 8 sn; ABBA dört
çift) makine yükü düşükken ilk kesimlerde +%10–25 gösterdi — bu
performans turunun tetikleyicisi. Son sürümde (yük 15–20): base 5,6 /
7,3 / 5,9 / 7,2 ↔ HEAD 5,4 / 5,8 / 5,2 / 5,8 — her çiftte HEAD daha
düşük (yük altında gürültü büyük; süreç içi ölçümler yük gürültüsünden
bağımsız ve yukarıda).

**Mutation-check'ler** (yedekten geri yüklenerek; her biri en az bir
testi kırdı): eski bayat kuralı (`unwrap_or(0)`); bayat hiç atılmıyor;
baseline'sız delta uygulanıyor; boşluklu delta düşüyor; upsert'ler
çıkışlardan önce (yeniden giriş + yeniden ekleme testleri); hücre
çıkışları yok sayılıyor; private full bayat denetiminden geçiyor;
private delta uygulanıyor; ilk geçiş hatası scratch'i bırakıyor; kayıt
gövdesi hatası yarım kare bırakıyor / baseline'ı koruyor; full önce
temizlemiyor; packed `removed` reddediliyor; oneof'ta ilk kol
kazanıyor; `sint32` 32 bite kırpmıyor; varint hızlı yolu `0x80`'i kabul
ediyor; onuncu bayt denetlenmiyor; yürüyücü hatadan sonra sürüyor;
kayıt aralığı son kaydın başında bitiyor / son kayıttan başlıyor. Demo
tarafında: eski `CellExit` okuması → yeni `delta_aoi` testi kırıldı.

**G3 için:** botlar `ClientView<D>`'yi oyun başına bir `ClientDecoder`
ile kullanır (demo: loadgen'deki `DemoDecoder`; MMO: test
istemcilerindeki `MmoDecoder`'ın elle yürüyen sürümü; arena yalnız full
gönderir, aynı görünüm onu da uygular). `LoadBot`'un §4.4'teki "kayıt
çözücü + çıkış-hücresi çözücü" parçası artık bu seam.

### G3 sonucu (2026-09-25)

**Tamam.** İki kod commit'i (+ bu belge), her biri kendi başına yeşil:
`e4e65a1` seam + demo botu (birebir) + `--game`; `99e786d` arena ve MMO
botları + raporlama + smoke. `gsb-core`, `gsb-kit`, `gsb-demo`,
`gsb-demo-arena`, `gsb-demo-mmo` **değişmedi** (`git diff 15a02c5.. --stat`
bu beşinde boş); wire baytları değişmedi. Test sayısı 609 → 623 (+1
bayrak kuralı, +3 arena botu, +4 MMO botu, +2 orkestratör komut satırı,
+4 `tests/loadgen_games.rs`; demo çözücüsünün iki testi `git mv` ile
taşındı; orkestratör komut satırının iki testi `child_args/tests.rs`
çocuğuna taşındı — ikisinin de iddiaları aynı); 1 ignored doctest aynı;
mevcut hiçbir iddia değişmedi (`loadgen_smoke.rs` dokunulmadı).

**API** (`gsb-server/src/loadgen/bot.rs`, ikiliye özel):

```rust
pub(crate) trait LoadBot: Send + Sync {          // oyun başına bir tane, dyn
    fn snapshot_op(&self) -> u16;                  // Game::SNAPSHOT_OP
    fn private_op(&self) -> u16;                   // Game::PRIVATE_OP
    fn client(&self, id: u64) -> Box<dyn BotClient>;
    fn flood_input(&self) -> (u16, Vec<u8>);       // numarasız (seq 0)
    fn churn_input(&self, id: u64, seq: u64) -> (u16, Vec<u8>);
    fn labels(&self) -> Option<Labels> { None }    // RESULT: visibility/shards/profile
    fn shard_spread(&self) -> bool { false }       // RESULT: shard_members=
    fn describe(&self) -> String;
}
pub(crate) trait BotClient: Send {                 // istemci başına
    fn apply_snapshot(&mut self, frame: &[u8]) -> Result<Snapshot, ClientError>;
    fn apply_private(&mut self, frame: &[u8]) -> Result<PrivateEvent, ClientError>;
    fn counters(&self) -> Counters;
    fn view_len(&self) -> usize;
    fn joined(&mut self, entity: u64) {}
    fn next_input(&mut self, elapsed: Duration, seq: u64) -> Option<(u16, Vec<u8>)>;
}
```

Görünüm `BotClient`'ın içinde: kit'in `ClientView<D>`'si oyunun
çözücüsüyle (`DemoDecoder`, `ArenaDecoder`, `MmoDecoder` — üçü de kaydı
`client::wire::Fields` ile elle yürür, üretilmiş çözücüye testle
sabitli). Loadgen'e özgü generic bir görünüm YAZILMADI. Alıcı döngüsü
kare başına bir sanal çağrı yapar; bağlantı, auth/join, tempolu
gönderim + sınırlı alım, girdi numaralandırma, ack takibi, leave ve
rapor oyundan bağımsız kaldı. `next_input` `None` dönerse o aralıkta
gönderim yok ve numara tüketilmez (demo'nun yerleşmiş still
istemcisi; arena/MMO'nun kendi entity'sini henüz görmemiş botu).

**`--game demo|arena|mmo`** (varsayılan `demo`): botu seçer; süreç içi
ve `--serve` sunucusunun `game` anahtarıdır; orkestratör onu sunucu
çocuğuna VE her istemci çocuğuna iletir (sunucu çocuğunun komut satırı
da artık saf bir kurucu, `server_args`, istemcininkinin yanında —
ikisi de süreç başlatmadan test ediliyor). Bilinmeyen oyun →
derlenmiş oyunları listeleyen hata. Demo'ya özgü bayraklar
(`--visibility`, `--topology`, `--shard-count`, `--cell-size`,
`--vision-radius`, `--spawn-half-size`, `--disconnect-grace-secs`,
`--profile`, `--still-frac`) başka bir oyun için yazılırsa, sıra ne
olursa olsun, bayrağı ve nedenini adlandıran hata — sunucunun "açıkça
yazılmış sabit anahtar reddedilir" kuralının loadgen yüzü. Orkestratör
bu bayrakları yalnız demo koşusunda iletir. `--help` güncellendi
(`[demo]` işaretli bayraklar; churn bayrakları da artık listede).

**Botlar:**

- **Demo** — bugünkü kod birebir (`bot/demo.rs`): `ring`/`spread`/`still`,
  `--still-frac`'ın id bölmesi, `as i32` kırpma, `spawn_home` kafesi
  (f64/f32 tuhaflığıyla), churn ve flood `MoveTo`'ları. **Bayt kanıtı:**
  15a02c5'in döngüsündeki girdi kodunu kopyalayan geçici bir sonda, bota
  karşı 576 000 aralıkta (üç profil × dört still oranı × iki harita × 400
  id × 60 aralık, düzensiz zamanlarla) her girdiyi, churn ve flood
  girdileriyle birlikte bayt-bayt AYNI buldu (commit'lenmedi).
- **Arena** — birim, kendi spawn noktasını (takım üssü, takım
  arkadaşlarının yanında) onu gösteren ilk takım snapshot'ından öğrenir;
  sonra ev → merkez → ev, 8 sn'lik tur (`s = (1 − cos φ)/2`), yükseklik
  0..20 m (`10·(1 − cos(φ + 1,3·id))`). Gerekçe: 25 m'lik üs halkasında
  hedef birimlerin 12 m/s'sinin altında kalır (birim hedefi izler);
  20 m'lik yükseklik salınımı 15 m görüş yarıçapını aşar, yani aynı zemin
  noktasındaki iki birim birbirini zamanın bir kısmında görür — üç
  takımın birimleri merkezde birbirinin 3D sisine gerçekten girip çıkar.
  "En yakın üs" değil kendi spawn'ı: wire takımı söylemiyor ve büyük
  takımlarda slotlar zemine kırpılıyor (500 istemcide x = 50'ye yığılır),
  en yakın üs yanlış olurdu.
- **MMO** — ilk girdi waystone `id mod 4`'e `Travel` (K4: katalogla
  barındırılan her oturum kaydısız, shard 0'da başlar; yalnız yürüyen
  bot dört shard'dan birini yükler — sonda: bu seyahat olmadan 8 botun
  6'sı shard 0'da kaldı). *K4 turunda kaldırıldı (`ae0e680`): bot
  `lg-{id}` loadgen'in barındırdığı realm'deki kayıtlı karakteriyle
  waystone `id mod 4`'ün halkasında başlar — aşağıda "K4 sonucu".* Sonra waystone çevresinde 30–60 m (id'ye göre)
  halkada 0,15 rad/s dolaşma (4,5–9 m/s, karakterin 7 m/s koşusu
  civarı; halka 64 m AOI hücrelerini keser — hücre çıkışları ve
  one-shot full'lar akar); ~20 sn'de bir başka bir waystone'a `Travel`
  (500 karakterde saniyede 25 göç — MMO'da uzun yolculuk ara sıradır,
  yükün çoğu yürüyen karakterlerin AOI akışı kalır); menzilde
  (`ATTACK_RANGE` = 30 m, 3D) bir mob varken ~1 sn'de bir `Attack`
  (sıradan bir yakın dövüş temposu; standart realm'de her waystone'un
  yanında bir kamp ve devriye gezen bir sürü var, halka ikisinin de
  menzilinden geçer). Oranlar `--move-ms`'ten girdi başına olasılığa
  çevrilir; çekilişler id tohumlu SplitMix64 akışı (tekrarlanabilir,
  bölümlemeden bağımsız). Üç mesaj tek sıra uzayında numaralı.

**Raporlama.** RESULT her anahtarını korur. Kendi düzeni olan oyunlarda
`visibility=`, `shards=`, `profile=` oyunun düzenini söyler (arena:
`team`/`1`/`base-centre`, MMO: `spatial`/`4`/`roam` — komut satırının
varsayılan `all`'ı yanıltıcı olurdu) ve `still_frac=0`. MMO
`shard_members=a,b,c,d` ekler (kararlı pencerenin SONUNDA shard başına
üye; shard başına örnek satırlarından, sıra = shard indisi) —
`game=`'den hemen önce, yani önceki her anahtar yerinde ve `game=` son
anahtar. İnsan-okunur rapor pencerenin başındaki ve sonundaki dağılımı
basar (`server shards (members per shard, steady window): first=…
last=…`). Demo'nun satırı değişmedi; CLIENT satırı her oyunda aynı.

**Demo A/B** (15a02c5 ↔ HEAD, dönüşümlü üç tur, `GSB_LOADGEN_CLIENT_LINES=1`;
makine yükü 19–23): RESULT anahtar kümesi, sırası ve değer biçimleri
(tamsayı / ondalık basamak) dört senaryoda da AYNI, CLIENT anahtarları
AYNI; her koşuda `left=N errors=0 server_closes=0`.

| Senaryo | `snap_total` base / HEAD | `client_in_bps` base / HEAD | `out_bps_per_conn` | `acks` | `step_p50_fine_us` |
|---|---|---|---|---|---|
| `50 --duration 3` | 4150 4104 4100 / 4103 4100 4100 | 561633 561133 560466 / 562400 560533 553926 | 11005–11029 / 10989–11044 | 800 / 800–802 | 48–80 / 40–80 |
| `50 --duration 3 --visibility spatial` | 4079–4092 / 4088–4136 | 103541–106313 / 107380–109260 | 1920–1977 / 1941–1978 | 800 / 800 | 104–152 / 104–152 |
| `50 --topology sharded --visibility spatial --shard-count 4 --duration 8` | 11462–11524 / 11460–11505 | 155033–157658 / 155658–156857 | 2894–2940 / 2901–2941 | 2298–2299 / 2297–2298 | 56–80 / 48–80 |
| `--orchestrate 1000 --procs 2 --visibility spatial --duration 8` | 225077–226064 / 223958–226282 | 64,2–64,6 M / 63,3–64,7 M | 65063–65444 / 64090–65432 | 44537–44815 / 44263–44780 | 992–1208 / 992–1288 |

`fulls`/`private_fulls`/`deltas`/`gap_drops`/`view_size` de gürültü
içinde (ör. spatial `deltas` 3924–3936 / 3931–3934; orkestre
`gap_drops` 856–877 / 863–867). **Alıcı döngü yavaşlamadı:**
`client_in_bps` aynı; orkestre `clients_cpu_s` ilk üç çiftte (b, h)
(5,8, 5,8) (4,8, 5,0) (6,2, 6,5) — aynı koşularda kodu DEĞİŞMEYEN
sunucunun `server_cpu_s`'i de HEAD'de 0,1–0,2 yüksekti (yük gürültüsü);
ek dört ABBA çiftinde (yük 13–17) HEAD 5,8 / 5,5 / 5,1 / 5,5 ↔ base
6,4 / 5,9 / 6,3 / 6,6 — her çiftte HEAD düşük ya da eşit.

**İlk ölçüm tabanları** (release, 32 çekirdek, `--duration 10
--write-stall-secs 0` — CHANGELOG "Ölçüm kaydı": aksi hâlde write-stall
koruması doymuş loadgen istemcilerini keser ve ölçüm sunucunun kesme
politikasını ölçer). Komutlar: `gsb-loadgen N --game G --duration 10
--write-stall-secs 0` (N = 50, 200, 500) ve `gsb-loadgen --orchestrate
1000 --procs 2 --game G --duration 10 --write-stall-secs 0`. Makine
başka işlerle yüklüydü; koşu öncesi 1 dk yük ortalaması tabloda. Her
koşuda `joined = left = N`, `errors=0`, `server_closes=0`,
`server_hz=30.00`, `step_over_budget_pct=0.0`.

| Oyun | N | Mod | Yük | step p50/p90 fine (µs) | step max (µs) | `out_bps_per_conn` | `snap_total` | acks / moves | peak payload (B) / `snap_overflows` | Not |
|---|---|---|---|---|---|---|---|---|---|---|
| arena | 50 | in-proc | 12,6 | 104 / 144 | 282 | 10 669 | 14 600 | 2900 / 2950 | 444 / 0 | records/tick 101,6 (overlap 2,03) |
| arena | 200 | in-proc | 11,0 | 368 / 456 | 757 | 46 049 | 57 419 | 11 514 / 11 514 | 1954 / 715 | records/tick 447 |
| arena | 500 | in-proc | 9,7 | 1104 / 1640 | 3363 | 111 508 | 138 091 | 27 451 / 27 561 | 5162 / 798 | records/tick 1115 |
| arena | 1000 | sep (2 süreç) | 5,0 | 1632 / 2024 | 3551 | 234 120 | 285 809 | 56 737 / 56 737 | 10 267 / 905 | `server_cpu_s` 2,8, `clients_cpu_s` 8,1, `dropped` 1761 |
| mmo | 50 | in-proc | 8,6 | 48 / 80 | 235 | 5 465 | 14 237 | 2863 / 2900 | 702 / 0 | shard üyeleri 14,13,11,12 → 14,11,13,12 |
| mmo | 200 | in-proc | 7,8 | 128 / 176 | 634 | 20 739 | 56 885 | 11 375 / 11 483 | 1276 / 0 | 57,44,50,49 → 55,49,50,46; `gap_drops` 155 |
| mmo | 500 | in-proc | 7,0 | 224 / 296 | 1632 | 49 096 | 136 178 | 27 193 / 27 354 | 2610 / 4229 | 134,120,121,125 → 125,111,136,128; `gap_drops` 398 |
| mmo | 1000 | sep (2 süreç) | 5,1 | 288 / 400 | 1540 | 102 190 | 280 645 | 55 775 / 56 155 | 9282 / 4746 | 253,254,246,247 → 245,245,255,255; `gap_drops` 991; `server_cpu_s` 2,3, `clients_cpu_s` 7,2, `dropped` 1836 |

Arena yalnız full gönderir (`deltas=0`); MMO'da kareler çoğunlukla delta
(500: 130 805 delta, 6589 full, 1614 private full). `ack_lag_max_ms`
arena 34–48, MMO 100–115 (bir `Travel`'ın ack'i hedef shard'dan göçten
sonra gelir — K1 düzeltmesi). Orkestre `dropped` (fan-out'ta dolu çıkış
kanalı) aynı boyuttaki demo koşusuyla aynı mertebede (A/B'de base
1538–1943). MMO'da oyuncular dört shard'a eşit yayılıyor (±%10).

**Bulgular:**

- **G3-1 — arenanın full snapshot'ları ~150 birimin üstünde rUDP MTU'sunu
  aşıyor.** Takım sisi, arena ölçeğinde (her takım nüfusun ~2,2 katı
  kayıt görüyor: `overlap_x` 2,0–2,2) ve yalnız-full politikasıyla,
  en büyük yük 200'de 1954 B, 500'de 5162 B, 1000'de 10 267 B
  (`max_snapshot_bytes` 1400; `snap_overflows` sayıyor, kareler yine
  gönderiliyor). TCP'de sorun değil; arena bir gün rUDP'de koşarsa
  takım odasına delta ya da parçalama gerekir. Kod değişmedi (kayıt).
  → **ÇÖZÜLDÜ, taşımada (rUDP parçalama turu, DESIGN §6 "MTU"):**
  rUDP artık bütçeyi aşan oyun bandı karesini FRAG datagram'larına
  böler, istemci birleştirir; kit ve çekirdek değişmedi. A/B
  (`--transport udp --stagger-ms 5`): arena 200'de yazıcının attığı
  grup datagram'ı ~45 500 → 0 (45 533 mesaj 91 066 parçayla gitti,
  `frag_dropped=0`), arena 500'de ~100-124 k → 0; istemci başına
  full sayısı 5×/16× arttı (botlar artık kendini görüyor). Arena 500
  rUDP'de adım p50/p90 aynı koşuda TCP'ninkiyle aynı aralıkta
  (968-1120/1408-1840 ↔ 1104-1136/1632-1760 µs). `snap_overflows`
  sayılmaya devam ediyor — artık rUDP'de parçalanma sinyali, kayıp değil.
- **G3-2 — join'de grup delta'ları one-shot private full'dan önce
  gelebiliyor** (`gap_drops`, kayıpsız TCP'de): MMO 200/500/1000'de
  istemci başına ~0,8, demo'nun orkestre spatial 1000'inde de aynı
  (A/B base 856–877) — yani önceden vardı, bot kaynaklı değil;
  50 istemcide 0. Görünüm kuralı doğru davranıyor (baseline'sız delta
  düşer, full gelince iyileşir). Sıralamanın core/kit'te garanti edilip
  edilmeyeceği ayrı bir soru; bu turun kapsamı dışında.
  **İncelendi — küçük paket; kod değişmedi (bilinçli).** *Sıra
  nereden geliyor:* çekirdeğin fan-out'undan, kit'ten değil. Oda ve
  shard aktörünün 4d adımı bağlantının batch'ine önce grubun paylaşılan
  karesini, SONRA `GameLogic::private`'ın karesini koyar
  (`room/actor/snapshot.rs`, `shard/actor/snapshot.rs`); kit yalnız iki
  gövdeyi yazar, batch'i görmez. Join tick'inde yerleşik bir grubun
  karesi delta'dır → baseline'sız istemci onu düşürür (`gap_drops`) →
  aynı batch'teki one-shot full baseline'ı kurar. `kit.proto` bu sırayı
  zaten sözleşme olarak yazıyor ("same batch right after the new
  group's delta").
  *Deney (geri alındı):* shard fan-out'unda batch'i ters çevirip private
  kareyi öne almak, MMO 200'de (`--duration 10 --write-stall-secs 0`)
  `gap_drops` 147 → 0 verdi — ama `deltas` da 55 168 → 54 454 düştü
  (−714): aynı delta hâlâ gönderiliyor, yalnız artık full'ın AYNI
  sequence'ından sonra geldiği için `stale` olarak atılıyor (loadgen
  `stale`'i raporlamıyor; düşüş katılan + hücre geçen istemcilerin o
  tick'teki delta'ları). Yani sıra değiştirmek bir sayaç yeniden
  adlandırmasıdır, düzeltme değil.
  *Neden yapılmadı:* (1) sırayı çevirmek wire değişikliğidir — ack'li
  her tick'te HER oyunun HER bağlantısında kare sırası değişir (bu
  paketin kapısı: arena dışında bayt değişmez); yalnız one-shot full
  tick'lerinde çevirmek için çekirdeğin kareyi tanıması gerekir, oysa
  private gövde çekirdeğe opak. (2) Gerçek düzeltme o bağlantıya grup
  karesini HİÇ göndermemek olurdu (full onu kapsıyor: aynı tick, üst
  küme) — bu hem wire değişikliği (bir kare eksik) hem çekirdek API'si
  (`private`'ın "bu kare grup karesinin yerine geçer" sinyali) ister;
  kazanç join/geçiş başına bir delta karesi. Tetikleyici: `gap_drops`
  temiz bir kayıp sinyali olarak gerekirse (rUDP'de gerçek kayıpla
  karışır) ya da bu kareler bant ölçümünde görünür hale gelirse.
- **G3-3 — arena istemcisi takımını wire'dan öğrenemiyor.** JOIN sonucu
  yalnız wire id veriyor; takım (ve dolayısıyla üs) istemci tarafında
  ancak kendi spawn konumundan çıkarılabiliyor. Bot bunu yapıyor; gerçek
  bir arena istemcisi için arena protokolüne bir takım alanı (ör. ilk
  private karede) eklemek düşünülebilir — korunan crate, bu turda
  dokunulmadı.
  **Kapandı — küçük paket:** kit'e oturum yükü kancası geldi
  (`Game::session_private` → `Private.game = 4`, oturum başına bir kez:
  join ve resume sonrası ilk private kare; istemcide
  `ClientDecoder::session_private` — KIT-ARCHITECTURE §5.1 "Oturum
  yükü"; BACKLOG A6'nın tetikleyicisi buydu). Arena orada
  `Welcome { uint32 team = 1; uint32 teams = 2; }` gönderiyor
  (`arena.proto`; takım 0 varsayılan olduğu için `team` yazılmaz, alan
  4 yine gelir); kasıtlı ve tek wire değişikliği bu, bayt bayt kilitli
  (`gsb-demo-arena/tests/wire.rs`: takım 1/3 → `22 04 08 01 10 03`).
  Diğer oyunların baytları değişmedi (varsayılan kanca hiçbir şey
  yazmaz; kit'te altı odada, demoda `kit_wire`). Arena botu artık
  evini `Welcome`'dan (`base_of(team)`, `teams` ile) alıyor, spawn
  konumundan tahmin etmiyor; hoş geldin gelene dek girdi göndermez.
- **K4 hâlâ geçerli**: yük dağılımı botun ilk `Travel`'ına dayanıyor
  (kaydısız oturum → shard 0). Kalıcı çözüm çekirdekte/yönlendiricide.
  **Kapandı — K4 turu** (aşağıda "K4 sonucu"): çekirdek yönlendiriciye
  ve join kancasına doğrulanmış kimliği veriyor; 200 MMO botu ilk
  `Travel` olmadan `shard_members=53,50,50,47`.

Korunan crate'lerde eksik public bir şey çıkmadı: botların ihtiyacı
olan her şey public (`WAYSTONES`, `ATTACK_RANGE`, `client_cell`,
`to_dm`/`to_cm`, opcode'lar, proto tipleri; `gsb_server::games::mmo::
DEFAULT_WAYSTONE`).

**Mutation-check'ler** (yedekten geri yüklenerek, her biri bir testi
kırdı): istemci çocuğuna `--game` iletilmiyor → `both_children_get_the_game`
+ `loadgen_orchestrates_the_mmo`; sunucu çocuğuna iletilmiyor → aynı
ikisi; MMO'nun ilk `Travel`'ı yok → `the_first_input_disperses_the_population`
+ `loadgen_drives_the_mmo` (dağılım iddiası bu sondayla sıkılaştırıldı:
önce "≥3 dolu shard" gevşekti, 6,1,0,1 geçiyordu; artık shard 0 ≤ 4/8);
bayrak denetimi kapalı → `a_demo_flag_refuses_another_game` +
`loadgen_refuses_a_wrong_game_line`; arena botu evini hiç bulmuyor →
`inputs_run_between_home_and_centre_once_home_is_seen` +
`loadgen_drives_the_arena`.

**§4.4 taslağından sapmalar:** (1) seam iki trait: aile (`LoadBot`) ve
istemci (`BotClient`) — görünüm istemci başına, oyunun somut
`ClientView<D>`'si onun içinde, böylece döngü generic değil ve kare
başına tek sanal çağrı; (2) "kayıt çözücü + çıkış-hücresi çözücü" G4'ün
`ClientDecoder`'ı oldu (ayrı bir loadgen seam'i yok); (3) raporlama
için iki isteğe bağlı kanca (`labels`, `shard_spread`) — taslakta yoktu.

### K4 — oyuncu kimliği → ev shard'ı (tasarım, 2026-09-25)

**Sorun.** Bulgu K4 (yukarıda, G2) ve KIT-ARCHITECTURE Faz 4 gözlem 1:
join yönlendiricisi (`BuiltRoom::Sharded::home_shard`) ve oyunun spawn
kancası (`GameLogic::on_join` → kit'in `Game::spawn_player`) yalnız
taşıma oturumunu (`ConnectionId`) görüyor. Bağlantı kimlikleri kabulde,
kabul sırasıyla basılıyor; MMO'nun realm'i kayıtlı karakterleri bu
kimlikle anahtarlıyor — barındırılan sunucunun asla dolduramayacağı bir
anahtar. Sonuç: gerçek sunucuda HER MMO oturumu kaydısız → shard 0,
waystone 0; gerçek istemcilerle sharding tek shard'a iner. Yükü yalnız
loadgen botunun ilk `Travel`'ı (waystone `id mod 4`) yayıyor (G3 "K4
hâlâ geçerli").

**Taşıyıcı: zaten var olan doğrulanmış kimlik (`identity: String`).**
Çekirdek, join anında oyuncunun doğrulanmış kimliğini ZATEN taşıyor —
resume anahtarı olarak: bağlantı aktörünün `identity`'si (ticket yolunda
`ValidatedTicket.player`, ticket'sız eski yolda `Auth.name`) →
`RegistryMsg::SpawnPlayer.identity` → `RoomOp::Join.identity` →
`ShardMsg::Join.identity` / `RoomControl::Resume.identity` →
odanın `admit_fresh(identity)`'si. Yalnız son adım eksik: kimlik
yönlendiriciye ve oyunun join kancasına verilmiyor. Yeni tip ya da yeni
mesaj alanı gerekmiyor; boş dize = anonim (bugünkü anlamı).

`PlayerId` doğru taşıyıcı DEĞİL: join'in ÇIKTISI (oyun `on_join`'de,
oda/shard-yerel sayaçtan basar), her taze oturumda yenidir (yalnız park
defteri onu resume boyunca sabit tutar), yönlendirme anında henüz yoktur
ve oda dışında anlamı yoktur — bir karakter veritabanını anahtarlayamaz.
İlişki: kimlik → (park defteri) → `PlayerId` → entity; kimlik oyuncunun
kalıcı adı, `PlayerId` oturumunun oda içi anahtarı.

**Karar (çekirdek seam'i — ince, saf, senkron):**

1. `BuiltRoom::Sharded::home_shard: Arc<dyn Fn(ConnectionId, &str) ->
   usize + Send + Sync>` — yönlendirici doğrulanmış kimliği de alır.
   Registry onu join dispatch'inde bugünkü gibi çağırır (hiç await
   edilmez; registry kuralı).
2. `GameLogic::on_join_as(&mut self, world, conn, identity: &str) ->
   Admission` — varsayılanı `on_join(world, conn)`. Oda (`admit_fresh`)
   ve shard (`ShardMsg::Join`) artık bunu çağırır. `on_join` zorunlu
   kalır: mevcut her mantık (çekirdek testlerinin ~25 stub'ı, demo, kit)
   değişmeden derlenir; kimliği isteyen mantık `on_join_as`'ı ezer. Kit'in
   zaten kullandığı desen (`ingest_seam` → `ingest`, `spawn_team_player`
   → `spawn_player`).
3. Kit: `Game::spawn_player_as(&mut self, world, conn, identity) ->
   Entity` — varsayılanı `spawn_player`. Kit odaları (`OpenRoom`,
   `AoiRoom`, `SectorRoom`, `ShardedRoom`, `ShardedSpatialRoom`)
   `on_join_as`'ı uygular ve `spawn_player_as`'ı çağırır; `on_join`
   onlara boş kimlikle iner. Takım odası kapsam dışı: spawn'ı
   `TeamGame::spawn_team_player(world, conn)`, kimlik almaz (kayıtlı
   karakterli bir takım oyunu tetikleyici; bugün yok).
4. MMO: `Realm::logins` doğrulanmış kimlikle anahtarlanır
   (`with_login(name, pos)`); `MmoGame::spawn_player_as` kaydı olanı
   kaydına, olmayanı (ya da anonimi) bugünkü varsayılana (shard'ın
   waystone'u) koyar; sunucu modülünün yönlendiricisi aynı tabloyu
   kimlikle okur (kayıt → kaydın shard'ı, yok → `DEFAULT_WAYSTONE`'un
   shard'ı — §6 karar 6 değişmez, yalnız anahtar değişir).
5. Wire baytları DEĞİŞMEZ: kimlik zaten AUTH'ta geçiyor.

**Resume ile etkileşim — kavga yok.** Kimlikli bir join zaten önce
resume denemesidir: sharded odada `ShardMsg::Resume` BÜTÜN shard'lara
yayınlanır, park kaydını tutan (tek) shard kabul eder; yönlendirici
yalnız hepsi "burada değil" dediğinde (taze join) devreye girer. Yani
park edilmiş bir karakter park edildiği yerde (kaydının shard'ında
değil) devam eder; park bittiyse taze join kaydına iner. Test bunu
kaydından başka bir shard'a seyahat edip orada düşen bir karakterle
kilitler.

**Geliştirme yolu uyarısı (güven).** Karakter anahtarı, ticket-auth
yapılandırılmışsa ticket'ın doğrulanmış `player`'ıdır — platformun
kimliği, istemcinin iddiası değil. Ticket'sız eski yolda anahtar
istemcinin iddia ettiği `Auth.name`'dir: **herkes her karakter olarak
girebilir** (ve bugün zaten olduğu gibi onun park edilmiş karakterini
devralabilir). Bu yol yalnız geliştirme / demo / loadgen içindir;
üretimde ticket hook'u zorunludur (SECURITY §4b). Yeni auth mekanizması
eklenmez.

**Loadgen.** Botlar `lg-{id}` adıyla girer (eski yol). Loadgen'in kendi
barındırdığı MMO sunucusu (süreç içi ve `--serve` çocuğu) standart
realm + bir bot kadrosuyla kurulur: `lg-{id}` karakteri waystone
`id mod 4`'ün çevresindeki dolaşma halkasında kayıtlı. Botlar
yayılmış başlar; ilk `Travel` kaldırılır. Katalogla başlatılan bir
`gsb-server` (`--addr` ile hedeflenen) bu kadroyu bilmez — orada botlar
kaydısızdır (karakter veritabanı sunucunun işi; katalog realm'inde
kayıtlı karakter yok).

**Elenen alternatifler:**

- *JOIN'e karakter kimliği alanı* (`JoinRoom.character`): wire
  değişikliği; üstelik istemcinin seçtiği karakter doğrulanmamış bir
  iddia olurdu — hangi hesabın hangi karakteri oynayabileceği platformun
  kararı ve ticket'a zaten kodlanabilir (`player` = hesap/karakter).
- *Adın hash'iyle yönlendirme* (`hash(kimlik) % shard`): yükü yayar ama
  karakteri kaydından bağımsız bir shard'a koyar; yönlendirici ile
  oyunun spawn'ı ayrışır (kayıt A bölgesinde, join B shard'ında → ilk
  tick'te göç) ve konum hiç yerleştirilmez. Yerleşim oyunun kararıdır,
  çekirdeğin değil.
- *Arama servisi* (join'de karakter veritabanına async çağrı):
  `home_shard` saf ve senkron kalmalı (registry hiçbir şeyi await
  etmez). Bir platformun veritabanı turu join'den ÖNCE yapılır —
  ticket doğrulayıcısı bunun yeri; oyunun realm'i o verinin süreç
  içi kopyası. Seam iki durumda da aynı.
- *`PlayerId`'yi taşıyıcı yapmak*: yukarıda — join'in çıktısı,
  yönlendirme anında yok, oda-yerel.
- *`on_join`'in imzasını değiştirmek* (`on_join(world, conn,
  identity)`): davranış kazancı olmadan beş crate'te ~45 uygulama ve
  çağrı yeri; varsayılanlı ikili aynı sözleşmeyi verir.
- *`JoinInfo` yapısı*: tek alan için (kimlik); mevcut kancalar kimliği
  `&str` alıyor (`on_disconnect`, `resume_lookup`). İkinci bir alan
  (ör. ticket talepleri) doğarsa o gün.

### K4 sonucu (2026-09-25)

**Tamam.** Tasarım (`f8b91ec`, yukarıda) + dört kod commit'i (+ bu
belge), her biri kendi başına yeşil; önce kırılan testler,
mutation-check'ler yedekten geri yüklenerek (diff her seferinde geri
temiz). Wire baytları DEĞİŞMEDİ: `gsb-protocol`, `gsb-net`, `gsb-demo`,
`gsb-demo-arena` dokunulmadı (`git diff 0a1614a.. --stat` bu dördünde
boş); kimlik zaten AUTH'ta geçiyordu. Test sayısı 664 → 670 (+2
çekirdek, +1 kit, +2 barındırılan MMO, +2 loadgen botu, −1 yerine
geçen dağılma testi); 1 ignored doctest aynı.

| # | Commit | Değişiklik | Kanıtlayan test (önce kırıldı) | Mutation → sonuç |
|---|---|---|---|---|
| 1 | `888730a` | **Çekirdek seam**: `registry::HomeShard = Arc<dyn Fn(ConnectionId, &str) -> usize + Send + Sync>` (`BuiltRoom::Sharded::home_shard`'ın tipi); registry onu `(conn, &identity)` ile çağırır. `GameLogic::on_join_as(world, conn, identity)` — varsayılanı `on_join`; odanın `admit_fresh`'i ve shard'ın `ShardMsg::Join`'i onu çağırır. Toplam üç çağrı satırı + bir tip takma adı + bir varsayılanlı metot; mevcut hiçbir mantık değişmedi (yalnız üç test yönlendiricisi ve iki sunucu fabrikası closure'a `_identity: &str` ekledi) | `join_identity::a_sharded_room_routes_and_spawns_by_the_authenticated_identity` — GERÇEK bağlantı aktörleri + canlı registry: ticket yolunda (`Auth.name = "trinity"`, bilet `t-neo`) yönlendirici ve kanca `neo` görüyor, eski yolda `bob`, anonimde boş; her join kimliğinin shard'ına iniyor. `…a_single_room_hands_its_join_hook_the_authenticated_identity` — tek oda (resume geri düşüşü + düz join). İlk hâl: imzalar vardı ama `""` geçiyordu → ikisi de kırıldı (`neo` shard 0'a; kanca `""`) | registry `(group.home)(conn, "")` → sharded kırıldı; shard `on_join_as(…, "")` → sharded kırıldı; oda `on_join_as(…, "")` → tek oda kırıldı; oda `on_join` çağırıyor → tek oda kırıldı |
| 2 | `d7be317` | **Kit**: `Game::spawn_player_as(world, conn, identity)` — varsayılanı `spawn_player`. `OpenRoom`, `AoiRoom`, `SectorRoom` (ortak `common::join`, artık kimlik alıyor), `ShardedRoom`, `ShardedSpatialRoom` `on_join_as`'ı uygular; `on_join` onlara boş kimlikle iner. Takım odası değişmedi (`spawn_team_player` kimlik almaz). Test sarmalayıcıları kancayı iletir | `game::tests::every_game_spawning_room_hands_the_game_the_identity` (beş oda; fixture adlı girişi `Login` bileşeniyle işaretler). İlk hâl: kanca vardı, odalar çağırmıyordu → kırıldı ("open") | `common::join` → `G::spawn_player` → kırıldı (open); sharded `spawn_player` → kırıldı; spatial `inner.on_join` → kırıldı; aoi `""` → kırıldı |
| 3 | `06ac5da` | **MMO + sunucu modülü**: `Realm::logins: Arc<HashMap<String, Pos3>>` (doğrulanmış kimlikle; shard'lar tek tabloyu paylaşır), `with_login(name, pos)`, `saved(identity)`; `MmoGame::spawn_player_as` kaydı olanı kaydına, olmayanı / anonimi shard'ın waystone'una koyar; `games::mmo::route(realm, identity)`. MMO testleri anahtar değiştirdi (iddialar aynı; anonim join'ler `cN` adını aldı) | `mmo_home::the_ticket_player_picks_the_character_not_the_claimed_name` (ticket hook'lu gerçek sunucu: `bob` diyen istemci ann'in biletiyle ann'in karakterine, shard 1'e; bob'un bileti bob'unkine, shard 2'ye; kaydısız bilet shard 0'a, waystone 0'a); `mmo_home::a_resume_lands_on_the_parked_character_and_a_logout_returns_to_the_save` (kaydı shard 1'de olan ann waystone 2'ye gidip orada düşüyor: sonraki oturum park edilen karakteri shard 2'de, AYNI wire id ile, durduğu yerde resume ediyor ve oynuyor; 1 sn çıkış sayacı dolunca yeni oturum taze join — yönlendirici onu kaydına, shard 1'e gönderiyor); `game::tests::characters_spawn_at_their_saved_position` (aynı kimlik başka bağlantıdan da kaydına) | MMO spawn `""` ile arıyor → birim testi + `mmo_e2e` 2 test + `mmo_home` 2 test kırıldı; yönlendirici `""` → `mmo_e2e::joins_land…` + `mmo_home` 2 test kırıldı; çekirdekte ticket yolunda kimlik `auth.name` → ticket testi kırıldı; registry dispatcher'ı resume'u atlıyor (hep düz join) → resume testi kırıldı (yeni entity) |
| 4 | `ae0e680` | **Loadgen**: botun ilk `Travel`'ı kaldırıldı; loadgen'in barındırdığı MMO (süreç içi ve `--serve` çocuğu, `server::start_hosted`) `Realm::standard()` + bot kadrosu (`bot/mmo/roster.rs`: `lg-{id}`, `id < 65 536`, waystone `id mod 4`'ün dolaşma halkasının başında). Bot `at = id mod 4` ile başlar; churn girdisi ev waystone'unun yanında; bot adı tek yerde (`bot::bot_name`) | `loadgen_games::loadgen_drives_the_mmo` — ilk hâl: `Travel` kaldırılmış, sunucu katalogdan → kırıldı (`shard_members=6,0,2,0`); `loadgen_orchestrates_the_mmo` artık dağılımı da istiyor; bot birim testleri `the_roster_saves_every_bot_on_its_home_ring`, `every_bot_roams_its_home_waystone_from_the_first_input` (eski `the_first_input_disperses_the_population`'ın yerine) | `--serve` katalogdan → orkestre testi kırıldı (`4,0,0,0`); süreç içi katalogdan → `loadgen_drives_the_mmo` kırıldı (`6,0,2,0`) |

**Yük ölçümü** (release, `gsb-loadgen 200 --game mmo --duration 10
--write-stall-secs 0` — G3 tabanının komutu; makine paylaşımlı, başka
bir ajan kardeş worktree'de derliyordu, 1 dk yük ortalaması tabloda):

| Koşu | Yük | `shard_members` (ilk → son, kararlı pencere) | step p50/p90 fine (µs) | step max (µs) | `out_bps_per_conn` | `snap_total` | acks / moves | peak payload (B) / `snap_overflows` | `gap_drops` | `server_hz` |
|---|---|---|---|---|---|---|---|---|---|---|
| G3 tabanı (ilk `Travel`) | 7,8 | 57,44,50,49 → 55,49,50,46 | 128 / 176 | 634 | 20 739 | 56 885 | 11 375 / 11 483 | 1276 / 0 | 155 | 30,00 |
| K4 #1 (kadro) | 5,02 | 56,43,52,49 → 53,50,50,47 | 120 / 160 | 265 | 21 422 | 57 304 | 11 500 / 11 505 | 903 / 0 | 155 | 30,00 |
| K4 #2 (kadro) | 4,55 | 58,42,49,51 → 54,49,50,47 | 104 / 152 | 493 | 21 151 | 56 552 | 11 302 / 11 305 | 896 / 0 | 155 | 30,00 |

Her koşuda `joined = left = 200`, `errors=0`, `server_closes=0`,
`step_over_budget_pct=0.0`. Dağılım ve bütün sayılar G3 bandında:
**ilk-`Travel` bayrağı TUTULMADI** — G3 tabanlarıyla karşılaştırma
için gerekmiyor (kararlı pencere aynı yerleşimi görüyor; fark yalnız
ilk saniyedeki bot başına bir göç, ölçüm penceresinin dışında).

**Tasarımdan sapmalar:** (1) `Realm::logins` `Arc`'lı paylaşılan
tablo oldu (tasarım yalnız anahtarı söylüyordu): 65 536 karakterlik
kadro her shard'a ayrı kopyalanmasın diye; `Realm::saved(identity)`
okuyucu. (2) Loadgen'in flood girdisi (`--flood-id`) hâlâ waystone 0'a
yürür — flood, düşürme korumalarını ölçer, yerleşimi değil; flooder
artık evinden oraya yürür.

**Açık kalanlar / bulgular (kod değişmedi):**

- **Katalog realm'inde karakter yok.** `gsb-server` ikilisi
  (`game = "mmo"`) hâlâ herkesi varsayılan waystone'a koyar — kayıtlı
  karakter kaynağı (karakter veritabanı / kalıcılık) sunucunun işi;
  seam hazır, veri yok (PERSISTENCE). `--addr` ile katalog sunucusuna
  sürülen loadgen botları bu yüzden shard 0'da başlar.
- **Çıkışta kayıt yok.** Realm statik: çıkış yapan karakter bir sonraki
  taze join'de kaydına döner (son konumuna değil) — `mmo_home` bunu
  sabitliyor. Konumu çıkışta yazmak kalıcılık turunun işi.
- **Takım odası kimliği oyuna iletmiyor** (`TeamGame::spawn_team_player`
  kimliksiz). Tetikleyici: kayıtlı karakterli bir takım oyunu (W paketi
  bunu isteyebilir).
- **Eski yol güveni** (SECURITY §4b): ticket'sız sunucuda karakter
  anahtarı istemcinin iddiası.

## 6. Kararlar (ebeveyn, kullanıcının "hepsini tamamla" talimatıyla)

1. **Config yeri:** demo'nun anahtarları eski düz yerlerinde kalır (geriye
   uyumluluk); yeni oyunlar **kendi adlarını taşıyan** bir tablo kullanır
   (`[arena]`, `[mmo]`). `Config`'in demo alanları bu işte silinmez.
   *G1 düzeltmesi:* ilk karar `[game]` tablosuydu; ama `game = "arena"`
   dizesi (karar 3) ile `[game]` tablosu aynı TOML belgesinde aynı
   anahtarı iki kez tanımlar, birlikte var olamaz. Oyun adını taşıyan
   tablo çakışmayı yapısal olarak kaldırır ve hangi tablonun hangi oyuna
   ait olduğunu da okunur kılar.
2. **Oyunun sabitlediği bir anahtar açıkça yazılmışsa:** başlatma hatası.
   Repodaki kural zaten bu (desteklenmeyen kombinasyonlar başlatmada
   reddedilir); sessiz yok sayma bir operatörü yanıltır.
3. **Oyun seçimi:** config'te `game = "demo" | "arena" | "mmo"`, loadgen'de
   `--game`; varsayılan `demo`.
4. **Adaptörlerin yeri:** `gsb-server` içinde, özelliklerin arkasında.
   `--no-default-features` derlemesi CI kapısı olur.
5. **Modülün motor anahtarlarını geçersiz kılması:** modül, sabitlediği bir
   motor anahtarını (ör. MMO'nun kendi park politikası varken
   `disconnect_grace_secs`) açıkça yazılmışsa reddedebilir; yazılmamışsa
   kendi değerini kullanır ve `describe()`'da gösterir.
6. **MMO join yönlendirmesi:** kayıtlı karakteri olmayan oturumlar,
   `spawn_player`'ın bugünkü geri düşüşüyle tutarlı olarak, varsayılan
   durak taşının shard'ına gider. Çekirdeğin `on_join`'a hesap kimliği
   vermemesi (KIT-ARCHITECTURE Faz 4 gözlem 1) bilinen sınır olarak
   kalır; bu işte çekirdeğe dokunulmaz. *K4 turunda kapandı:*
   yönlendirici (`HomeShard`) ve join kancası (`on_join_as` →
   `spawn_player_as`) doğrulanmış kimliği alıyor; geri düşüş aynı
   (kaydısız ya da anonim → varsayılan durak taşı).
7. **Servis yaşam döngüsü:** bugünkü gibi — servis görevi, göndericileri
   düştüğünde biter. Yeni bir durdurma protokolü bu işin kapsamı dışında;
   kayda geçirilir.
8. **`LoadBot`:** `dyn` (mesaj başına bir sanal çağrı, loadgen için
   önemsiz). Arena profili: takımlar üslerinden merkeze ve geri hareket
   eder (sis sınırlarını gerçekten geçer). MMO profili: yürüme + ara sıra
   `Travel` (shard'lar arası) + `Attack` karışımı.
9. **Generic istemci görünümü:** G3'te loadgen'de doğar, G4'te
   `gsb_kit::client`'a taşınır. *G4 düzeltmesi (ebeveyn):* sıra
   tersine döndü — G4 G3'ten önce koşuldu, görünüm doğrudan kit'te
   doğdu; G3'ün botları loadgen-yerel bir generic görünüm büyütmeden
   onu kullanır.
10. **Orkestratörün `--cell-size`'ı istemci süreçlerine iletmemesi:**
    hata olarak düzeltilir (G1, ayrı commit, testle). İstemci görünümü
    varsayılan 20'yi kullanırken sunucu iletilen değeri kullanıyordu;
    yalnız varsayılan olmayan hücre boyutuyla orkestre edilen spatial
    koşuları etkiler.
11. **Ekonomi asimetrisi** (servis 6 oda tipinin yalnız 3'üne bağlı):
    düzeltilir, servis her demo odasına bağlanır. Faz 1b'nin "her kit
    odası isteği oyuna iletir" kararının doğal devamı; AOI/team/pvs
    odaları ECONOMY'ye artık "servis yapılandırılmadı" yerine gerçek cevap
    verir. Kasıtlı davranış değişikliği olarak CHANGELOG'a yazılır.
12. **`RoomKind` ve eksen hataları** demo modülüne taşınır; `ServerError`
    modül hataları için bir `Game(...)` varyantı kazanır.

## 7. Elenen alternatifler

- **Oyun başına tamamen ayrı kompozisyon kökü (C):** her oyun ~470 satırlık
  başlatma mantığını (yarım başlamama sırası, durdurma sırası, metrik
  yönlendirme) yeniden yazardı.
- **Adaptörleri oyun crate'lerine koymak (saf A):** `gsb-demo` →
  `gsb-server` → (test) `gsb-demo` dev-bağımlılık döngüsü; loadgen ikilisi
  `gsb-server`'da kalamaz; oyun crate'leri sunucu yığınına bağımlı olurdu.
- **Tek ikili, trait'siz büyük `match` (saf B):** bağlılığı büyütür,
  üçüncü taraf oyun için yol açmaz.
- **Trait'i generic yapmak (`GameModule<W, G, St, Sp>`):** generic'ler
  config ve boot'a viral olarak yayılırdı; registry-başlatma sınırı bunu
  gereksiz kılıyor.
- **Yalnız bayt düzeyinde ölçen loadgen:** spatial odalar için bütün delta
  ve görünüm kontrollerini kaybettirirdi.
