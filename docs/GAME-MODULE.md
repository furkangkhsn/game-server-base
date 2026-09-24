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
  `ClientView<Cell>`'e toplanır.
- **Demo botu bugünkü kodun birebir taşınmasıdır** (profiller, `as i32`
  kırpma, seq numaralandırma), yani girdileri bayt-bayt aynı kalır.

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
| G3 | Loadgen: `LoadBot` + generic görünüm, demo botu birebir, `--game` her iki çocuğa iletilir, arena ve MMO botları, ilk ölçüm tabanları | demo için RESULT/CLIENT birebir (+`game=`); arena/MMO ilk sayılar |
| G4 | Kit istemci kurallarının `gsb_kit::client` modülüne alınması (tetikleyici var: üç kopya) + çözücü seam'i; demo ve MMO test istemcilerinin ona geçirilmesi | test iddiaları değişmez |

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
Yönlendirme (§6 karar 6): kayıtlı karakteri olan oturum kaydının
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
   kalır; bu işte çekirdeğe dokunulmaz.
7. **Servis yaşam döngüsü:** bugünkü gibi — servis görevi, göndericileri
   düştüğünde biter. Yeni bir durdurma protokolü bu işin kapsamı dışında;
   kayda geçirilir.
8. **`LoadBot`:** `dyn` (mesaj başına bir sanal çağrı, loadgen için
   önemsiz). Arena profili: takımlar üslerinden merkeze ve geri hareket
   eder (sis sınırlarını gerçekten geçer). MMO profili: yürüme + ara sıra
   `Travel` (shard'lar arası) + `Attack` karışımı.
9. **Generic istemci görünümü:** G3'te loadgen'de doğar, G4'te
   `gsb_kit::client`'a taşınır.
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
