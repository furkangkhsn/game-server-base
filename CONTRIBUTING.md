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

CI'daki JavaScript action'ları, `runs.using`'i node24 olan ana sürüme
sabitlenir (GitHub Node.js 20 çalışma zamanını kaldırıyor; node20
sürümleri uyarı verip zorla Node 24'te koşuyordu — B34):
`actions/checkout@v5`, `actions/upload-artifact@v6`.
`dtolnay/rust-toolchain` composite action'dır (Node yok);
`Swatinem/rust-cache`'in `@v2`'den yeni ana sürümü yoktur, `@v2`
sürümleriyle birlikte ilerler. Yeni bir `uses:` eklenirken aynı kural
geçerlidir.

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

## Gerçek saatli testler (BACKLOG F23, F25)

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
- **Duvar saatiyle hız sınırlanmış bir cevabı yoklayan test**
  (bağlantı aktörünün saniyede en çok bir HEARTBEAT_ACK'i) tek atımlık
  yoklama yapmaz: son cevaplanandan "yaklaşık bir saniye sonra" giden
  tek yoklama yükte pencerenin içine düşer ve cevapsız kalır. Yoklama
  cevaplanana dek yinelenir ve yalnız KENDİ numaralı isteklerinin
  cevabını kabul eder; süre sınırı yine yalnız asılma korumasıdır
  (F24: `afk_action.rs::still_answered`).
- **Duvar saatinde sabit bir pencerede ölçülen hız performans
  iddiasıdır**, doğruluk değil: ticker takılmadan sonra patlamaz, saate
  yeniden oturur; aç kalan süreç dürüstçe daha az adım atar. Böyle bir
  koşu yalnız takılmanın değiştiremeyeceğini iddia eder: yapılandırmanın
  yankısı (odanın kendi örneğindeki `budget_us`), "en az bir adım" ve
  üst sınır (T süren bir koşuda en çok `hz × T + 2` adım); hızın kendisi
  paused saatte sabitlenir (F25: `gsb-server/tests/loadgen_rate`).
- **"X sürerken" bir koşuldur, pencere değil**: yavaş kancayı (ticket
  doğrulayıcısı) test serbest bırakana dek park et, girişini bildirsin;
  sokette bekleyen eski kareleri saymamak için aynı çıkış kuyruğundan
  geçen bir çit kullan (F25: e2e ticket testi — girişten sonra
  gönderilen heartbeat'in ACK'ı).
- **Rapor bekleyen test raporun neyi saydığını bekler**: "1,1 sn uyu,
  son raporu oku" değil, gönderilen her kareyi saymış ilk rapor
  (`frames_in == gönderilen`; eksikse bağlantılar flush aralığından
  sonra dürtülür — F25: `gsb-server/tests/input_rate.rs::settle`).
- **Motorun saati `std::time::Instant` ise** (yazma-takılma saati, rUDP
  istemcisinin RTO'su) paused saat işe yaramaz: bileşenin saat alanı
  geri sarılır, uyunmaz (F25: busy-band testi — kareler okumadan önce
  sokette bekler). Bu olmuyorsa pencere sahte ucun temposundan çok büyük
  tutulur ve "birkaç pencere sürer" hızdan değil yapıdan gelir
  (slow_reader'lar: 1 sn pencere, 8–10 ms'de ≤ 1 KiB okuma, çerçeve en
  az 384 okuma).
- **Sabit `sleep` yalnız ALT sınır olarak** kullanılır (flush aralığını,
  1/sn ACK kısmasını geçmek): fazla uyumak iddiayı bozmaz. Üst sınır
  (`took < …`) yalnız iddianın kendi sınırıdır (el sıkışma süresi;
  paused testte sanal süre), makinenin hızı değil (F25: `accept_stop`,
  `boot::stop` paused saatte `took == grace`).
- Yeni gerçek saatli bir test yük altında denenir: 32 çekirdekte
  `for i in $(seq 30); do sh -c 'while :; do :; done' & done`, sonra
  `cargo test --workspace --no-fail-fast` birkaç kez (ve
  `--features gsb-server/otlp`), bitince `kill $(jobs -p)`. Meşgul
  döngüler bütün bir sürecin donmasını (swap, cgroup kısması) üretmez —
  sayım turu 5'in 1,61 Hz'i de öyleydi (128 `yes` altında loadgen 30 Hz
  kalıyor); onu test ikilisini periyodik durdurup sürdürerek üret:
  `while :; do pkill -STOP -f '^<ikili>'; sleep 0.25; pkill -CONT -f
  '^<ikili>'; sleep 0.25; done` (bitince `pkill -CONT`). F25'te
  loadgen smoke'ları, slow_reader'lar, `input_rate` ve e2e ticket testi
  bununla her seferinde ya da çoğunlukla düştü.

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
