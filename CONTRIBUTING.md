# Katkı Rehberi

Bu dosya `docs/HANDOFF.md` ve `README.md`'deki bağlayıcı disiplinin kısa
özetidir; çelişki görürsen o iki belge geçerlidir.

## Ortam

- Toolchain `rust-toolchain.toml` ile sabitlenmiştir (Rust **1.95.0**,
  `rustfmt` + `clippy`); MSRV (`Cargo.toml` → `rust-version`) aynı
  sürümdür — `bevy_ecs 0.19.1` 1.95.0 ister.
- Build **sistemde `protoc` istemez**: `gsb-protocol`, `gsb-kit` ve
  `gsb-demo` build script'leri `protoc_bin_vendored::protoc_bin_path()`'i
  `Config::protoc_executable` ile `prost-build`'e verir; açık yol olduğu
  için `PROTOC`/`PATH` aramasının önüne geçer (yanlış bir `PROTOC` build'i
  kıramaz). Gömülü ikilinin bulunmadığı bir hedefte `cargo:warning`
  basılır ve `PROTOC` → `PATH` aramasına düşülür; orada sistem `protoc`'u
  gerekir.
- Yeni bağımlılık indirilecekse cargo komutlarının başına
  `CARGO_HOME=$PWD/.cargo` ekle (HOME önbelleği salt-okunur olabilir;
  `.cargo` gitignore'dadır).

## Kapılar (CI'da da koşar: `.github/workflows/ci.yml`)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # 0 uyarı
cargo test --workspace                                   # tamamen yeşil
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps   # 0 rustdoc uyarısı
cargo build -p gsb-server --lib --no-default-features    # oyunsuz sunucu çekirdeği
cargo build -p gsb-server --lib --no-default-features --features game-arena   # tek oyun
cargo build -p gsb-server --lib --no-default-features --features game-mmo     # tek oyun
cargo build -p gsb-server --lib --no-default-features --features game-war     # tek oyun
cargo build -p gsb-server --features otlp                                      # OTLP exporter'ı (varsayılan kapalı)
cargo clippy --workspace --all-targets --features gsb-server/otlp -- -D warnings   # 0 uyarı
cargo test -p gsb-core --features otlp --lib && cargo test -p gsb-server --features otlp --test otlp_export
```

Feature'sız derlemeler (`--no-default-features`) Prometheus exporter'ını
da dışarıda bırakır (`prometheus` varsayılan feature'lardandır): yukarıdaki
oyunsuz/tek oyun satırları onu da derler.

CI ayrıca `autobahn` işini koşar: WS kapısına karşı Autobahn fuzzing
client'ı (`docs/SECURITY.md` §3.7; yerelde docker imajı gerekir).

Tur sonu ayrıca:
`cargo run --release -p gsb-server --bin gsb-loadgen -- 50 --duration 3`
(ve `--game arena`, `--game mmo`, `--game war` ile) → `left=50`, `errors=0`, panik yok.
Loadgen'in reddettiği bir komut satırı panik değildir: stderr'de tek
satır (`gsb-loadgen: <sebep>`) ve çıkış kodu 2; `--help` stdout + 0.

## Yasak desenler (`gsb-lint`)

Her crate'in `build.rs`'i `gsb_lint::check`'i çağırır; `src/`, `tests/`
ve `examples/` altındaki `.rs` dosyalarında şu desenler **build'i kırar**:
`tokio::select`, `futures::select`, `select!`, `Mutex`, `RwLock`,
`parking_lot`.

Neden: mimari kilitsiz ve hot path'te çoğullamasızdır. Aktörler yalnız
kendi mailbox'larına `recv()` eder, G/Ç ayrı pump görevlerindedir; tüm
paylaşılan durum aktörlere aittir ve kanallarla değiş tokuş edilir. Kural
kod incelemesine değil derleme zamanına bağlıdır.

Dikkat: yorumlar taramadan önce sökülür, ama **string literal'ler
korunur** — bu kelimeleri string'e bile yazma.

## Aktör disiplini

- Aktörler **tek-awaited**: döngünün tek await'i mailbox `recv()`'idir
  (oda aktörü için global tick `recv()`'i); tick gövdesi senkrondur.
- Kanallar **bounded**; hot path'te `try_send` / `try_recv` (dolu kanal
  girdiyi düşürür — o bağlantının izolasyonu).

## Çalışma döngüsü

1. İddiayı koddan **doğrula**.
2. **Düzelt**.
3. **Davranış-kilitleyici test** ekle (mümkünse mutation-check: düzeltmeyi
   geri al, testin kırıldığını gör).
4. `clippy` → 0 uyarı.
5. Tüm süit yeşil.
6. Kommit.

`#[ignore]` ekleme, test silme ya da testi gevşetme yok.

## Gerçek saatli testler (BACKLOG F23)

Yükte düşen testlerin ortak kalıbı: gerçek saatte sabit bir pencere
ve içinde zamanlayıcının kaç kez uyandığına bağlı bir sayım. Kural
(ayrıntı: `docs/TICK-ARCHITECTURE.md` "Tick saati", F23):

- **Tick/adım sayan ya da motor süresinin dolmasını bekleyen test
  paused saatte koşar** (`#[tokio::test(start_paused = true)]`) — o
  saat `gsb_core::ticker::now()` ise (ticker, grace/tavan, RPC zaman
  aşımı, girdi hız sınırı, girdi-boşta saati).
- **Duvar saatine bağlı bir şeyi** (metrik toplayıcının rapor periyodu,
  bağlantı aktörünün pencereleri, loadgen son tarihleri) **sayan test
  "`sleep(D)`, sonra `>= N`" yazmaz**: koşulu bekler; süre sınırı yalnız
  asılma korumasıdır (ör. 10 sn), iddianın parçası değildir.
- **Test düzeneğinin sahte ucu** (peer, tick beslemesi) ölçülen kodun
  gerçek-zamanlı penceresinin darboğazı olmaz.
- Yeni gerçek saatli bir test yük altında denenir: 32 çekirdekte
  `for i in $(seq 30); do sh -c 'while :; do :; done' & done`, sonra
  `cargo test --workspace --no-fail-fast` birkaç kez (ve
  `--features gsb-server/otlp`), bitince `kill $(jobs -p)`.

## Kod düzeni

- Hedef dosya boyutu 200-250 satır; dosyayı büyütmek yerine alt modüle böl.
- Bir struct'ın impl'ini bölerken **KARDEŞ değil ÇOCUK modül** kullan:
  çocuk modül atasının private alanlarını görür, alanlar `pub`/`pub(crate)`
  olmadan kapsülleme korunur.
- Hedefi aşan bir dosya bilinçli bir istisnaysa (tek blok olmak zorunda
  olan trait/trait impl, tek sürekli prosedür) gerekçeyi commit mesajına
  yaz.

## Git

- Yalnız **açık pathspec** ile `git add`. Bir ajan aktifken **asla**
  `git add -A` / `git add .`.
- Paralel ajanlar ayrı `git worktree`'lerde çalışır (aynı ağaçta iki ajan
  = dosya çakışması).
- Commit mesajları İngilizce ve emir kipinde; kod yorumları İngilizce,
  `docs/` Türkçe.

## Dokümanlar

- Her tur `docs/ROADMAP.md` durum satırını ve `docs/CHANGELOG.md`'yi
  günceller.
- Tasarım kararından önce **elenen alternatifler** yazılır.
- Tetikleyicisiz optimizasyon yapılmaz ("önce veri").
