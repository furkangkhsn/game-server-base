# YENİ OTURUM DEVİR-TESLİM PROMPT'U

> Aşağıdaki metni yeni oturuma olduğu gibi yapıştır. Bu dosya kendisi de
> o oturumun okuyacağı bağlamdır.

---

Sen gsb ("game-server-base") Rust workspace'inde çalışacaksın:
`/home/furkangkhsn/Documents/Projects/Self/game-server-base`. Branch: main.
1811 test yeşil (2 ignored: doctest + elle koşan tıkanıklık ölçümü), clippy 0 uyarı, ağaç temiz. Görevin, sözleşmeli turları
devam ettirmek ve disiplini korumak.

Teknik borç turu (CHANGELOG "teknik borç turu") üç borcu kapattı ve
**dört maddeyi ürün kararına bıraktı**. Bunlardan **ikisi kapandı**
(CHANGELOG "bağlantı sınırları turu"): tıkanmış yazmaya süre sınırı
(`write_stall_secs`, ilerleme tabanlı, vars. 10 sn) ve post-auth
HEARTBEAT_ACK kısması (§3.2 eşiği auth sınırının ötesine taşındı).
**Üçüncüsü de kapandı** (CHANGELOG "AFK sinyali + girdi-boşta tavanı
turu"): AFK artık base'in kararı DEĞİL — base koşulsuz bir SİNYAL
yayınlıyor (`TickCtx::since_input`, "aksiyon taşıyan kare" yapısal
tanımı) ve varsayılan KAPALI bir TAVAN sunuyor
(`max_idle_input_secs`); tavan dolduğunda kararı oyunun
`on_disconnect`'i veriyor, base kendiliğinden despawn etmiyor. Detach /
park / bot etkileşimi: `docs/RECONNECT.md` §16.

**Dördüncüsü de kapandı** (CHANGELOG "E1 + F21"): geçerli girdiye hacim
limiti opt-in bir yapı taşı olarak var — bağlantı başına token bucket
(`RoomConfig::input_rate`, varsayılan KAPALI; sayı oyunun ya da
config'in). Aşan girdi bağlantı aktöründe düşer, `input_rate_limited`
sayılır, ihlal değildir. SECURITY §3.4.

O turun açık yan bulgusu (`step_fine_hist` shard aktöründe hiç
yazılmıyor) **kapandı** — bkz. CHANGELOG "park sızıntısı + shard metrik
boşluğu turu". Aynı tur, bir öncekinin yarım kalan park sızıntısını da
kapattı: registry satırı artık politika park etmeyi REDDETTİĞİNDE de
bırakılıyor (`disconnect_grace_secs = 0`, yani varsayılan, tam olarak bu
koldur); `RegistryMsg::ParkExpired` → `DetachDespawned`.

Onun yerine geçen açık yan bulgu (`step_min_us` / `late_min_us`
minimum değil) da **kapandı** — bkz. CHANGELOG "minimum sayaçlar turu".
Kullanıcı kararı ONARIM oldu: alanlar gerçek minimum yapıldı,
`step_first_us` yeniden adlandırması elendi. Muhasebe iki aktörden
`RoomCounters::observe_late_us`/`observe_step_us` çocuk modülüne alındı —
yeni bir süre sayacı eklerken oraya ekle, aktörlerin `lifecycle.rs`'ine
değil. İlk gözlemin iki ucu da SEED etmesi bir tuzak koruması: sıfırdan
başlayan bir minimum sonsuza dek 0 kalır.

Onun yerine geçen yan bulgu (`fold_rooms` minimumlar DIŞINDA da eksik
katlıyor) da **kapandı** — bkz. CHANGELOG "metrik fold denetimi turu".
Artık her alanın katlama kuralı kararlaştırılmış, koda yazılmış ve
DESIGN §12'ye işlenmiştir; kural YAPISALDIR: fold döngüsü
`RoomReport`'u tam destructure eder, yani rapora alan eklemek kuralı
yazılana kadar **derlemez** (E0027). Yeni bir metrik alanı eklerken üç
yer seni zaten derlemeyi kırarak uyarır: iki aktörün `sample()`'ı,
toplayıcının `RoomReport` literal'i ve `fold_rooms`'un destructure'ı.
Denetimin iki yan bulgusu da kapandı: akümülatör shard 0'ı HER toplamda
iki kez sayıyordu (50 istemcilik sharded koşu `members=61` diyordu) ve
loadgen'in ince-histogram percentilleri yanlış nüfusa soruluyordu
(`steps` MAX, histogramlar SUM ile katlanır → `folded_steps`).
**Açık yan bulgu kalmadı.**

Sonraki tur (CHANGELOG "stall gözlemlenebilirliği + bayt-granüler
ilerleme turu"): sunucunun başlattığı her kapanış artık sebebiyle
sayılıyor (`ServerClose`, `gsb_net_server_closes_total{reason}`,
loadgen `server_closes=`) ve write-stall saati kare değil BAYT ölçüyor.
Yeni bir sunucu-kapanış yolu eklersen ona bir `ServerClose` sebebi ver
(`ConnIn::ServerClosed { cause, .. }`) — istemci-tarafı son ve shutdown
bilerek sayılmaz. ~~Bekleyen: 10k A/B ölçümü (`c2b7160` ↔ `debdac9`).~~
*Kapandı — ölçüm alındı (CHANGELOG "Ölçüm kaydı"; ROADMAP "Oturum
zaman aşımı" maddesi; e-posta düzeltmesinden sonra commit'ler `c2b7160`
↔ `d8d9030`): fark gürültü içinde.*

**Kaynak ağacı yeniden düzenlendi** (okunabilirlik turu): 40 dosya →
207. Her modül kendi dizini; hedef dosya boyutu 200-250 satır. Bir
dosyayı büyütmek yerine alt modüle böl — ve bir struct'ın impl'ini
bölerken KARDEŞ değil ÇOCUK modül kullan (çocuk atasının private
alanlarını görür, kapsülleme bozulmaz). Hedefi aşan 44 dosya var ve
her biri bilinçli: trait/trait-impl tek blok olmak zorunda, ve tek
sürekli prosedürü ikiye bölmek yarı kurulmuş durumu modül sınırından
geçirmek demek. Yeni bir istisna eklersen commit mesajında gerekçelendir.

**gsb-kit tasarımı onaylandı ve Faz 0 tamam** (`docs/KIT-ARCHITECTURE.md`
§10, CHANGELOG "gsb-kit Faz 0 turu"): `gsb-game/src` `kit/` (stratejiler
+ ortak makine) ve `demo/` (örnek oyun) olarak bölündü; kit'in demo'ya
her erişimi geçici bir seam'den geçiyordu ve bir kaynak-tarayan test
kuralı kilitliyordu. *(Faz 2'de seam silindi, iki modül iki crate oldu —
aşağıda.)*

**Faz 1a tamam** (KIT-ARCHITECTURE §4.5 ve §10 "Faz 1a sonucu",
CHANGELOG "gsb-kit Faz 1a turu"): §4 trait'leri (`kit::codec`,
`kit::space` + `Grid2`, `kit::game`) var, `OpenRoom<G>` ve
`AoiRoom<G, S>` generic, demo `DemoGame`/`DemoCodec` ile uyguluyor.
`WireId`'yi yalnız kit'in `Minter`'ı kurabilir. Generic bir odada
`World::clear_trackers`'ı yalnız kit çağırır (`update` sonunda bir kez);
bir `Game` kancası çağırırsa debug'da panik.

**Faz 1b tamam — Faz 1 bitti** (KIT-ARCHITECTURE §4.6 ve §10 "Faz 1b
sonucu", CHANGELOG "gsb-kit Faz 1b turu"): bütün odalar oyun üzerinden
generic (`TeamRoom<G, V>`, `SectorRoom<G, M>`, `ShardedRoom<G, P>`,
`ShardedSpatialRoom<G, P, S>`); strateji-özel oyun kancaları `Game`'in
uzantılarında (`TeamGame`, `ShardGame`). Kit'in 2D ön-ayarları
(`Grid2`, `VisionGrid2`, `ConvexSectors2`, `GridPartition2`) oyunun
tiplerini yalnız `Planar` erişimcisiyle okur — yeni bir ön-ayara somut
konum tipi sokma. §8.2–§8.5 kapandı; oyun kodunun despawn'ları
silinen-bileşen tamponundan okunuyor, yani `clear_trackers`'ın tek
sahibinin kit olması artık doğruluk şartı. Bütün kit odaları istekleri
`Game::handle_request`'e yönlendirir. Seam'de test dışı yalnız kit
zarfı (`Private`, `InputAck`) kaldı — Faz 2'de kit proto'suna geçti.

**Faz 2 tamam — crate bölmesi** (KIT-ARCHITECTURE §5.1 ve §10 "Faz 2
sonucu", CHANGELOG "gsb-kit Faz 2 turu"): workspace'te `crates/gsb-kit`
(stratejiler, delta motoru, sharded kompozitler, park/resume,
ön-ayarlar, kendi `kit.proto`'su) ve `crates/gsb-demo` (eski
`gsb-game`, örnek oyun) var. **Kit hiçbir oyunu görmez, testlerinde
bile:** kit'in testleri kendi fikstür oyununda (`gsb-kit/src/testing/`)
koşar; `gsb-kit`'e bir `gsb-*` oyun crate'i bağımlılığı eklemek ya
cargo döngüsüdür ya da (dev-dependency) kit'in manifest testini kırar.
Bir kit testi bir oyunun kodeğinin YAZDIĞI değere (çözülmüş koordinat,
nicemleme) bakıyorsa o test oyunun crate'ine aittir (demo'da
`src/demo/rooms/tests/`). Kit'in zarfı `gsb.kit`'tir; demo'nun
`game.proto`'su onun tipli aynasını taşır ve `tests/kit_wire.rs` ikisini
aynı baytlara kilitler — zarfa alan eklersen iki tarafı birlikte
güncelle. Demo odalarının kurucuları uzantı trait'leridir: kullanan
dosyaya `use gsb_demo::prelude::*;`.

**Faz 3 tamam — 3D arena demosu** (KIT-ARCHITECTURE §10 "Faz 3
sonucu", CHANGELOG "gsb-kit Faz 3 turu"): `crates/gsb-demo-arena`,
kit'in kabul testi — yalnız kit'in public yüzeyiyle yazılmış 3D takım
sisi (`TeamRoom<ArenaGame, VisionGrid3<Pos3>>`, üç takım, yükseklik
sayılır), kendi hareketi / kodeği (santimetre) / proto'su (opcode'lar
1100..=1102). **`gsb-kit` ve `gsb-core`'a dokunulmadı.** Bir demo'nun
kit'te değişiklik gerektiren her ihtiyacı bir TASARIM BULGUSUDUR
(§11): kit'i yamama, iç öğeleri kopyalama — kaydet. Faz 3'ün iki
engelleyici olmayan bulgusu orada (takım spawn'dan sonra soruluyor;
kit zarfının istemci kuralları demo'nun proto'sunda). Arena sunucuya ve
loadgen'e bağlı değil (§12); testleri gerçek oda aktörü üzerinden.
**Faz 4 tamam — 3D MMO demosu, kapanış doğrulaması** (KIT-ARCHITECTURE
§10 "Faz 4 sonucu", CHANGELOG "gsb-kit Faz 4 turu"):
`crates/gsb-demo-mmo` — shard'lı dünya üzerinde yer-düzlemi ızgara AOI
(`ShardedSpatialRoom<MmoGame, GridPartition2<Pos3>, Grid2>`, `Planar` =
`[x, z]`), oyun kodunun spawn/despawn ettiği hızsız mob'lar, park → AI
devri, `mmo.proto` (opcode'lar 1200..=1204); entegrasyon testleri
gerçek dört `ShardActor` üzerinden. Beş korunan crate'e dokunulmadı;
üç demo aynı, Faz 2'den beri değişmemiş kit üzerinde yeşil
(`cargo test -p gsb-demo -p gsb-demo-arena -p gsb-demo-mmo`). **Uygulama
fazları bitti.**

**Güncel iş sırası ve bırakılanlar: `docs/BACKLOG.md`** (D, K4, U,
küçük paket, S, H, takım odasında delta, W1, W2 ✅; §1'in paketleri bitti — sıradaki iş BACKLOG §2'den).

**İki paralel hat (2026-09-28, kullanıcı kararı):** (1) rUDP sertleştirme
— B4+B2 → B1 → B3 → B7 → B5 (DTLS; kütüphane kararı kullanıcıda);
(2) kit — A8 → A5 → A7 → A9. Her tur kendi worktree'sinde; birleştirmeden
önce diğer hattın son hâline rebase + tam kapılar.

**A7 tamam** (CHANGELOG "A7", KIT-ARCHITECTURE §10 "A7"): bölme
ön-ayarlarında wire ölçeği — `GridPartition2/3::with_wire_scale(s)`;
`admits` ve `debug_check_wire` wire'ı ölçeğe bölerek okur, varsayılan 1 =
bugün. Hiçbir demo kullanmıyor. Açık: A38. Kit hattında sıradaki: A9.

**A9 tamam** (CHANGELOG "A9", KIT-ARCHITECTURE §10 "A9"): oyuncu başına
aydınlık — `LitGame::{light, lit}` + `LitAoiRoom` (`AoiRoom::…lit()`); ışığı
olan izleyici `LitGroup::Viewer(p)` grubunda yalnız aydınlık kayıtları
alır, ötekiler paylaşılan hücre paketini. Hiçbir demo kullanmıyor. Açık:
A39 (shard'lı mekânsal kompozitte ışık), A40. Kit hattı (A8 → A5 → A7 → A9)
bitti.

**Altı paralel tur (2026-10-02, kullanıcı kararı):** r2 (rUDP sinyalleri:
B86, alıcı raporu, B85, B87), t1 (yükte düşen testler: F51, F52, B88),
c1 (kapanış nedenleri: F60, F28, B30), s1 (kapı/ops sınırları: D11, B49),
m1 (F29, F64), g1 (demolar: B81, F26). Ajanlar yalnız hedefli test koşar;
tam kapıları ebeveyn sırayla koşar (kullanıcı kuralı).

**g1 tamam** (CHANGELOG "g1"): demo muharebe beslemesinin kayıpları
sayılıyor (`combat_hits_dropped_{full,closed}`, F17 kuralıyla yalnız >0);
MMO `Seam::find` + `Target` görünümüyle (savaşın deseni). Tel ve pinli
özetler değişmedi. Açık: F65 (`gsb_core::channel`'da sayan `try_send`).

**s1 tamam** (CHANGELOG "s1"): `max_handshakes_per_source` (vars. kapalı;
IPv4 adresi / IPv6 /64; QUIC'te kanıtlanmamış kaynak ayrı sayılır ve
sınırda Retry alır), ops HTTP `http_max_connections` (64) +
`http_write_timeout_secs` (10 sn). Dört yeni `gsb_transport_*` sayacı,
loadgen teli GSNG. Açık: B89 (rUDP kapısında kaynak sınırı), D12 (düz
TCP pre-auth), B90 (ops yönlendirmesinin süresi).

**c1 tamam** (CHANGELOG "c1"; F60, F28, B30): duruşun geçtiği hüküm
bağlantının sonunda bir kez sayılır; oda politikası kopan bağlantının hükmünü
`ConnectionClosedBy(ServerClose)` ile görür (kit ezmesi hükme göre); WS kapısı
kapanış kodunu oturumun sonuna göre seçer (`SessionEnd`, uç noktanın end
notice'i). Açık: F66–F69.

**m1 tamam** (CHANGELOG "m1"; DESIGN §12 "Toplayıcı uçuştaki turu bekler
(F29)", OPS §1/§2): toplayıcı, bir sharded odanın turu uçuştaysa raporu en
çok `metrics::CUT_GRACE` (250 ms) bekletir; rapor satırlar hizalanınca
(normalde bir sonraki tick) çıkar, sınırda eskisi gibi yırtık. Takvim tick
saatinde (`ticker::now`). Ölçüm: orkestre MMO'da yırtık 5/181 → 0/182.
Üst düzey ret artık `at <yol>:<satır>` der (`Config::origin`, opak
`ConfigOrigin`). Açık: F70, F71.

**t1 tamam** (CHANGELOG "t1"): gerçek saatli test kalıpları CONTRIBUTING
"Gerçek saatli testler"de (F52 maddeleri); `tests/hosted/ops.rs::until_metric`
odanın sayacını `/metrics`'ten okur (shard satırları `room << 16 | i`,
toplanır). Loadgen RESULT'ta `errors_*` anahtarları `errors=`'in hemen
ardında. `PROTOCOL_WAIT` (5 sn) rUDP'nin `REL_NO_ACK_FATAL`'ına elle
eşlenir (sabit `udp` modülünde gizli) — o değişirse
`loadgen/client/wait.rs` de. e2e aktif heartbeat testi 1 sn'den uzun
süreç donmasında tasarım gereği açık "kanıtsız koşu" mesajıyla düşer.
Açık: F72 (duvar saatli idle penceresi — karar), F73, F74, F75 (rUDP'nin
riskli testleri).

**rUDP sertleştirme 2 tamam** (CHANGELOG "rUDP sertleştirme 2", DESIGN §6):
`rel::HANDSHAKE_MAX_RTO` (200 ms) + `Rto::seed`; `udp::kernel` →
`udp_datagrams_dropped_kernel`; `udp::feedback` (PROBE 5 / REPORT 6,
`UdpClientConfig::game_reports` varsayılan açık, `UdpClient::connect_with`),
`GameEstimate`, 14 `udp_game_*` sayacı, `op::base::UDP_REPORT = 13`,
loadgen teli GSNJ. B85, B86, B87 kapandı; B1 tur 3'e daraldı. Açık: B91,
B93, B94, B96. Altı paralel turun hepsi birleşti (2026-10-02).

**Kullanıcı kararları (2026-10-02):** F72 — geç ateşlenen idle son tarihi
sürecin takılması sayılır; rUDP tur 3 — taşıma hızı ayarlar, gönderemediği
en eski oyun karesini sayarak düşürür, kit/oyuna sinyal (isteğe bağlı);
rUDP güvenliği — DTLS değil, **Noise NK + kendi kayıt katmanı** (ring'siz,
unsafe'siz; araştırma ve 10 karar `docs/RUDP-SECURITY.md`'de), IP
değişince oturum göçer (B3 = göç). Sıra: B3 → B89 → B5a → B5b → B7.
İkinci paralel dalga: r3, c2, w1, s2, x1 (şifreleme çekirdeği), e1 (B7
düz metin e2e).

**s2 tamam** (CHANGELOG "s2"): `max_unauth_conns_per_source` (vars.
kapalı; D11'in kaynak kuralı `gsb_core::source::Source`'ta paylaşıldı;
registry'de havuzun taramasıyla tek geçiş, ayrı tablo yok; red
`server_closes{reason="unauth_source_cap"}`), ops HTTP
`http_route_timeout_secs` (10 sn, aşılırsa 504,
`ops_http_routes_timed_out`). `RegistryMsg::ConnOpened`'a `source` alanı
eklendi. Loadgen teli GSNK. Açık: A41, B97, F76.

**w1 tamam** (CHANGELOG "w1"; B15, F66): write-stall saatinin üç
kalıntısından ikisi düzeltildi — TLS'te bayt sayısı rustls'in altından, TCP
akışını saran `gsb-net/src/wire.rs` `Wire`'dan gelir; WS'de soket yazıcı
görevi yazmanın anını kaydeder, pompa pencereyi bayttan başlatır. Çekirdek
uyanma histerezisi ölçülmüş ayar notu (SECURITY §3.5 satır 6, B98). Yazıcı
pompası saati `tokio::time::Instant`. F66: `ws_going_away_unsent_*` →
`ws_teardown_closes_unsent_*` (eski seriler durur, OPS §3).

**c2 tamam** (CHANGELOG "c2"): idle pencereleri artık sürecin takılmasını
istemcinin sessizliği saymaz (F72; `IDLE_STALL_GRACE` 250 ms, sessizlik
başına bir yeniden başlatma, `idle_windows_restarted_late`). Oyunlar
full/closed kaybını `gsb_core::channel::SendLosses` ile sayar (F65,
tokio'suz). Toplayıcının yırtık raporu `reports_torn_at_cut_grace` (F70).
Loadgen teli GSNL. Açık: B99–B102, F77, F78.

**e1 tamam** (CHANGELOG "e1"; B7 düz metin yarısı): `tests/rudp_resume.rs`
+ `rudp_resume/{rig,player,flows,grace}.rs`, RECONNECT §5 sonu. B3/B5
turları bu yedi testi yeşil tutmalı; B3 kendi beklentisini `flows.rs`'teki
kapı-başı beklentinin (`Door::drop_close`) yanına ekler.

**rUDP sertleştirme 3 tamam** (CHANGELOG "rUDP sertleştirme 3", DESIGN §6
"Tıkanıklık tepkisi"): `udp::congestion` (`Control`, `PaceQueue`,
`PathState`, `UdpCongestion`), `udp::writer::pace` (`wake()` = min(rto,
tick, hızlama)); `udp_congestion` (sunucu `UdpCongestionKind`, vars.
`"off"`), 5 `udp_game_*paced*` sayacı, `GameEstimate::{window_min_rtt,
interval_sent_bytes}`. Ölçüm düzeneği `udp::tests::pace::{relay, measure}`
(`--ignored`). Loadgen teli GSNM. B1, B91, B93, B96, F75 kapandı. Açık:
B103 (sinyali oyuna taşı — 4 karar), B104 (varsayılanı `"pace"`'e
çevirme), B105, B106.

**x1 tamam** (CHANGELOG "x1"): rUDP güvenliğinin çekirdeği
`crates/gsb-net/src/seal/` içinde ve testli, ama rUDP onu henüz
çağırmıyor. Sözleşme `docs/RUDP-SECURITY.md`: 10 kullanıcı kararı, tehdit
modeli, SEALED düzeni, göç kuralı, tur sırası. B5a bu modülü değiştirmeden
bağlamalı; değişiklik gerekirse doküman ve testler birlikte güncellenir.
İkinci paralel dalga (r3, c2, w1, s2, x1, e1) birleşti (2026-10-02).

**rUDP sertleştirme 1 tamam** (CHANGELOG "rUDP sertleştirme 1", DESIGN §6):
`gsb_net::listen::bind_udp` + `udp_recv/send_buffer_bytes`;
`udp::rel::{RelSend, Rto}` (RFC 6298, `[50 ms, 1 sn]`, Karn, boşta kalma
kuralı); `udp_control_retransmits_timeout`, loadgen teli GSNF. Bulgu:
geri çekilme varsayılan arabellekli katılma fırtınasında connect p99'u
~3,5× uzatıyor, ilacı B4 düğmesi — karar B86'da. B1 için: oyun bandı
ACK'lenmiyor, geri bildirim kararı gerekiyor. Açık: B85–B88.

**A5 tamam** (CHANGELOG "A5", KIT-ARCHITECTURE §10 "A5"): hacimsel AOI
`space::Grid3` ve 3B shard bölmesi `space::GridPartition3` (`[nx, ny, nz]`,
`with_diagonals()` → 26). İsteğe bağlı, hiçbir demo kullanmıyor. Açık:
A36, A37. Kit hattında sıradaki: A7.

**A8 tamam** (CHANGELOG "A8", KIT-ARCHITECTURE §10 "A8"): takım sisinde
birim başına görüş yarıçapı — `gsb_kit::team::SightRadius`, `Vision`'da iki
varsayılanlı metot, `space::MAX_SIGHT_CELLS = 4`. Hiçbir demo kullanmıyor.
`TeamMig`'e public `sight` alanı eklendi. Açık: A34, A35.

**F62 tamam** (CHANGELOG "F62", OPS §2 "Üst düzey", GAME-MODULE §4.3):
artık her config katmanı bilinmeyen anahtarı reddediyor — alt tablolar
ayrıştırırken (serde), üst düzey başlatmanın ilk adımında
(`Config::check_top_level_keys`). Üçüncü taraf modül `raw`'dan düz
anahtar okuyorsa `owned_keys()`'i ezmeli (vars. yalnız `[<ad>]`).
Derlenmemiş oyunun tablosu reddedilir. Açık: F64 (satır numarası).

**F63 tamam** (CHANGELOG "F63", OPS §2 "Başlatma hatası"): `gsb-server`
başlatmayı durduran hatayı `gsb-server: <hata>` olarak basıyor
(`gsb_server::error_chain`), çıkış durumu 1; `ConfigError`'ın `Debug`'u
dosyayı dökmüyor; `ServerError::Bind` → `ListenerBind { addr, transport,
source }` (kırıcı); loadgen süreç-içi reddi mesaj + 1.

**F61 tamam** (CHANGELOG "F61", OPS §2 "Kapı girdisi"): `[[listeners]]`
girdisi bilinmeyen anahtarı reddediyor (serde `deny_unknown_fields`;
hata anahtar + satır). Örnek dosyanın kapı girdileri dosyanın sonuna.
F62 ve F63 ayrı turlarda kapandı.

**B84 tamam** (CHANGELOG "B84", RPC-CONTROL-PLANE §8.2 "B84"): tokio'nun
bind'i 128'lik kuyruk veriyordu (1024 değil — mio ≥ 1.1). Motor:
`gsb_net::listen::bind_tcp` + sunucu anahtarı `listen_backlog` (vars.
128); loadgen `--listen-backlog N`. Ölçüm notu: katılma fırtınası
ölçülecekse `--listen-backlog` ≥ istemci sayısı; kayıttaki tabanlar
128'le alındı. F61 ayrı turda kapandı.

**Temizlik paketi tamam** (A12, F2, F58, F59): kit odaları dünya
sorgularını tutuyor (`common::Cached`, arketip nesli değişince yeniden
kurulum — kayıt sırası, dolayısıyla bayt aynı); registry `metrics_dropped`
ve `bytes_out` tam kilitli; registry'nin bağlantıya hükmünün spawn'lı
yedeği duruştan sonra reddedilirse sayılıyor (`registry/actor/tell.rs`,
`channel::post_or`; F60'ta sayım bağlantıya taşındı — `ShutdownOvertaking`, `post_or` yerine `post_where`).

**B50 tamam** (CHANGELOG "B50", RPC-CONTROL-PLANE §8.2 "B50"): pinsiz
orkestre tabanları varsayılan worker'larla yeniden ölçüldü, eskiler "tek
worker'lı çocuklar (B37 öncesi)" etiketiyle yerinde; sayım turlarından
regresyon yok (aynı gün A/B `a73bc54` ↔ `73da266`, süreç içi ve pinli
tabanlar). Ölçüm notu: koşu başı yük < 5 bekle; `--workers 1` eski
koşulu bugünkü kodda yeniden üretir. Açılan: B84, F59.

**Sayım turu 8 tamam** (CHANGELOG "Sayım turu 8", DESIGN §9/§12, OPS §3):
F54 `joins_unsent` (`conn/actor/room.rs`); F55 karar — B68'in
`leaves/detaches_unprocessed`'i duruşun defteri; F56
`metrics::VerdictsLost` (`registry/close.rs` flush'ları, `room/stop.rs`,
`registry/actor/run/leftovers.rs`, `conn/actor/unprocessed.rs`); F57
registry'nin bağlantıya bildirimleri `channel::post`. Loadgen teli GSNE.
Açık: F51, F52, F58.

**F53 tamam** (CHANGELOG "F53", DESIGN §9 "Registry'nin kutusunda
kalanlar", OPS §3): `registry/actor/run/leftovers.rs` — `Shutdown` kolu
kutuyu kapatır, kalanları sayar, son örneği `post` eder; `joins_unread`,
`team_exports_unread`, loadgen teli GSNC. F54 ve F55 sayım turu 8'de
kapandı.

**F30 + F34 + F50 tamam** (CHANGELOG "F30 + F34 + F50", CONTRIBUTING
"Gerçek saatli testler", OPS §3, DESIGN §9 "Duruşta takım export'u kapalı
kutuya"): `gsb-server/tests/loadgen_games/window.rs` (kanıtlı koşu);
`team_export_drops_{full,closed}` (metrik adı değişti), loadgen teli GSNB.
Açık: F51, F52 (F53 kapandı).

**F41 tamam** (CHANGELOG "F41", DESIGN §9 "Önce kapılar"): `boot/stop.rs`
— kapılar → `end_accepts` → `Shutdown` → ticker; testler
`boot/stop/tests/window.rs` (+ `window/rig.rs`). Yeni sayaç/tel yok.

**F35 tamam** (CHANGELOG "F35", DESIGN §12 "Son rapor üreticileri
bekler"): `metrics/collector/closing.rs` — kapanıştan sonra kanal
kapanışına dek katlama, `FINAL_REPORT_GRACE` 2 sn; oturum/taşıma kanalı
ayrımı `boot/start.rs`; `StopReport::final_report_complete`. Loadgen
`LEAVE_SETTLE` 150 ms. F33 ve F40 (`serve.rs::EXPORT_STOP_GRACE`) aynı
turda kapandı. F41 ayrı turda kapandı.

**F32 tamam** (CHANGELOG "F32", RECONNECT §5 "Kopuşu geçen yeniden
bağlanma"): `room::live_session` + oda/shard Resume kolunda devralma;
registry `players/handover.rs` (LEAVE yok, üyelik devri). **Sözleşme
değişti:** aynı kimlikle ikinci oturum eski varlığı alır (eskiden taze
varlık, eski `on_leave` ile giderdi). Yöntem notu (CONTRIBUTING): görevler
arası yarış süreç dondurmayla üretilmez, iş parçacığı düzeyinde aç
bırakmayla ve sırayı elle çevirerek üretilir. Açık: F35.

**F31 tamam** (CHANGELOG "F31", RPC-CONTROL-PLANE §8.2 "Sunucu çocuğunun
portları"): `--serve` stdout'a tek `SERVING addr=… metrics=…` satırı
basar (`serve/announce.rs`); orkestratör onu `procs/server_child.rs`'te
en çok 30 sn bekler, öteki satırları geçirir. `alloc_port` yok. Açık:
F40.

**Sayım turu 7 tamam** (CHANGELOG "Sayım turu 7"): `dispatch_resume`
shard oneshot'larını sınırsız bekler, toplama kanalı/aktarıcı yok
(RECONNECT §6 "Yavaş shard'a resume (B82)"). `ws_{close_frames,pongs}_
dropped_closed` (OPS §3, DESIGN §6). Loadgen teli GSNA. "Her kaybı say"
taraması tamam; açık yalnız B81 (demo).

**F25 tamam** (CHANGELOG "F25", CONTRIBUTING "Gerçek saatli testler"):
yük yöntemi — meşgul döngüler süreç donmasını üretmez; test ikilisini
SIGSTOP/SIGCONT ile periyodik durdur. Loadgen smoke'ları
`tests/loadgen_rate` ile yalnız takılmaya dayanıklı hız iddialarını sınar.
Açık: F30–F34.

**Sayım turu 6 tamam** (CHANGELOG "Sayım turu 6"): `OpOutcome::Refused`,
`MetricsEvent::JoinRefusedClosed` → `joins_refused_closed` (RECONNECT
§3.4/§6, OPS §3, DESIGN §12); duran shard kendi parkının resume'una
`RoomGone` der. WS kapanışı `ws/writer/going_away.rs` (`Teardown`),
`ws_going_away_unsent_*` (DESIGN §6, OPS §3). Loadgen teli GSMZ. B82/B83 sayım turu 7'de
kapandı.

**Sayım turu 5 tamam** (CHANGELOG "Sayım turu 5"): registry'de
`team_relays_dropped_{full,closed}` (OPS §3, CROSS-SHARD §8b), 
`udp_frames_drained` daraldı, kapı kapanışının sayaçları (DESIGN §6,
`intake/close.rs`, `udp/transport/queued.rs`). Loadgen teli GSMX. Açık:
B75, B80 (sayım turu 6), B81 (demo, istenirse).

**B71 tamam** (RECONNECT §6, CHANGELOG "B71"): `dispatch_resume`
fan-out'tan sonra `agg_tx`'i bırakıyor; katlama her shard cevap verince ya
da gidince bitiyor. Açık: B75 (dağıtıcının `RoomGone` katılmaları ve
parkı hiçbir shard'da olmayan kuyruktaki resume sayılmıyor).

**Sayım turu 4 tamam** (CHANGELOG "Sayım turu 4"): akış pompaları ve
rUDP'nin kalan kayıpları (OPS §3; `pump/lost.rs`, `pump::verdict` artık
rUDP'de de), `MetricsEvent::RoomEndedUncounted` (DESIGN §9/§12),
`metrics::StopCounts` (DESIGN §12, CROSS-SHARD §4b). RPC defteri 13
terim. Loadgen teli GSMV. Açık: B69–B74.

**Sayım turu 3 tamam** (CHANGELOG "Sayım turu 3"): yeni `transport`
kapsamı (DESIGN §6 sonu, OPS §3; `gsb_net::metrics::Flusher`,
`MetricsEvent::Transport`); duran oda `MetricsEvent::RoomFinal` gönderir
(DESIGN §12 "Duran odanın son sayımı"); bağlantının son örneği `post` ile
teslim edilir. RPC defteri 11 terimle kapanır (RPC-CONTROL-PLANE §8.3).
Loadgen teli GSMR. Açık: B66–B69.

**B61 tamam** (RECONNECT §3.4, CHANGELOG "B61"): reddedilen `Close` op'u
artık üyeliği sızdırmıyor — dağıtıcı kuyruğunun kapanması `Close`
sayılır; gitmiş dağıtıcıda registry `send_detach_direct`. B63 ve B64 küçük
paket 8'de kapandı; B65 de kapandı (dağıtıcı seri numarası;
RECONNECT §3.4, CHANGELOG "B65").

**Sayım turu 2 tamam** (CHANGELOG "Sayım turu 2", RPC-CONTROL-PLANE
§8.3, OPS §3): yeni sayaçlar — oda `requests_undelivered`,
`requests_abandoned`, `requests_dropped_unbound`, `actions_dropped_unread`,
`actions_dropped_unbound`; net `requests_dropped_full`, `requests_no_room`,
`heartbeats_throttled_{preauth,authed}`, `frames_out_closed`,
`close_notices_dropped`; registry `join_ops_dropped`, `close_ops_dropped`,
`match_results_dropped_{full,closed}`. **Anlamı daralanlar:**
`actions_dropped` (yalnız oyun girdisi), `frames_out`/`bytes_out_*`
(yalnız kuyruğa girenler), `shipped_*`/`private_frames` (yalnız kanalın
aldığı batch). RPC defteri §8.3'te her isteği tek terimde kapatır.
Loadgen teli GSMP. Açık: B58–B62 (B61 sızıntı).

**B52 tamam** (DESIGN §12 "tutarlı kesit", CHANGELOG "B52"): loadgen
nüfusu (`peak_members`, `shard_members=`, `steady_end`) yalnız oyunculu
tutarlı kesitlerden okunur; kesitleri yalnız boş oda olan koşu yırtık
geri düşüşe gider. Toplayıcının saniyelik yayını shard'ların örnek
tick'ine denk düştüğü için yükte kısa koşularda oyunculu kesit
çıkmayabilir (F29).

**Sayım turu tamam** (CHANGELOG "Sayım turu", RPC-CONTROL-PLANE
§8.2/§8.3): ilke "her şeyi saymalıyız" (bakımcı kararı). `dropped`
artık yalnız dolu çıkış kanalı; kapalı kanala deneme `sends_closed`.
Üyeliği oda bitirdikten sonra bağlantının kapalı kanala ilettiği kare
`requests_dropped_closed` / `actions_dropped_closed`. `release_actions`
okunmamış istekleri sayıyor. Loadgen metrik teli GSMJ. Açık: B53–B57
(sayılmayan kalan kayıplar).

**Küçük paket 7 tamam** (CHANGELOG "Küçük paket 7"): demo
`MovementSystem`'in birim testleri var (F1); demo'nun dört tek-dünyalı
odası da `with_economy` ile kurulur (F4). Açık: B52 — orkestre MMO
loadgen testi yükte bir kez `shard_members=0,0,0,0` verdi.

**B36 tamam** (RPC-CONTROL-PLANE §8.3, CHANGELOG "B36"): ayrılıştan hemen
önce gönderilip oda tarafından okunmamış istekler
`requests_dropped_unread` / `req_unread`'de; loadgen'in süreç içi koşusu
sunucuyu durdurmadan önce bir oda metrik periyodu bekler (koşular ~1 sn
uzun). Oda defteri: gönderilen = yanıtlanan + retler + `req_refused` +
`req_unread`. Loadgen metrik teli GSMH. Açık: oda üyeliği bitirdikten
sonra bağlantı aktöründe düşen istek (B51).

**F27 tamam** (RECONNECT §3.3, CHANGELOG "F27"): politika artık nedeni
bilir — `GameLogic::on_disconnect_with(world, player, identity, cause)`
(`DisconnectCause::{ConnectionClosed, IdleInput, Kicked}`,
`#[non_exhaustive]`; varsayılanı `on_disconnect`). Kit:
`with_disconnect_policy_for(cause, grace, to)` her oda türünde; ezme yoksa
her neden oda geneli kuralı alır. B43'te yeni üyelik `ConnectionClosed`.
Açık: `ConnectionClosed`'ın arkasındaki hükmü politikaya taşımak (F28).

**Küçük paket 6 tamam** (CHANGELOG "Küçük paket 6"): pinsiz orkestre
çocukları artık kendi varsayılan worker sayısında — **dikkat:** yeni
pinsiz orkestre sayıları eski tabanlarla doğrudan karşılaştırılamaz
(eskiler tek worker'lı çocuklarla alındı; liste RPC-CONTROL-PLANE §8.2,
yeniden ölçüm BACKLOG B50). Ops HTTP başlığı 5 sn içinde gelmezse tek
`408` + kapanış; yanıt yazma sınırı ve bağlantı tavanı yok (B49).

**B43 tamam** (RECONNECT §16.4, CHANGELOG "B43"): registry'nin
`on_close_conn`'u eşleşmeyen istekte tabloya dokunmaz ama hükmü yine
iletir; yeni üyelik bağlantının kapanışıyla (`ConnClosed` → DETACH →
`on_disconnect`) biter. Kabul edilen: aynı odaya doğrudan yeniden
katılmada eski üyeliğin sonu kümülatif `leaves`'te sayılmaz.

**E8 tamam** (RECONNECT §16.3, CHANGELOG "E8"): oyun mantığı
`ctx.kick(player, reason)` ile (kit: `gsb_kit::game::kick(world, entity,
reason)`) bir oyuncuyu sunucudan atar — `on_disconnect` yolu + E6'nın
kapatma fiili, ERROR 9 `kicked: …`, `server_closes{reason="kicked"}`.
E9 kararla kapandı (yeni bildirim yok). F27 ve B43 kapandı
(RECONNECT §3.3, §16.4).

**Küçük paket 5 tamam** (CHANGELOG "Küçük paket 5"): CI'da action'lar
node24 ana sürümlerinde (yeni `uses:` için kural CONTRIBUTING'de);
`/metrics` ve OTLP iki yeni oda ailesi taşır
(`gsb_room_shipped_frames_total`, `gsb_room_private_frames_total`);
loadgen'in `records_per_tick` penceresi tutarlı kesitte biter; `stop()`
artık hiçbir accept döngüsünü abort etmez (ops HTTP dahil — `http_listen`
açıkken `accept_loops_ended` dinleyici sayısı + 1).

**F6 tamam** (CHANGELOG "F6", KIT-ARCHITECTURE §10 "F6"): sharded bir
oyun alan etkisini / yakınlık sorgusunu `Seam::within` / `Seam::area`
ile (tek wire için `Seam::find`) kit'in öncelik kurallarıyla alır;
oyunun tek işi bir `SeamView` tipi (kendi entity'den ve ödünç kayıttan
aynı alanlar). `lent_iter` artık iki kiralayanlı wire'ı bir kez verir.
Savaş kullanıyor; MMO'nun "yerel, değilse ödünç" iki noktası aynı
yola çevrilebilir (BACKLOG F26).

**Küçük paket 4 tamam** (CHANGELOG "Küçük paket 4"): başlangıç odaları
`start` dönmeden registry mailbox'ındadır (testlerdeki "boot odası
oluşana dek bekle" döngüleri artık gereksiz ama zararsız); loadgen
nüfusu yalnız tam (her shard satırlı) kesitlerden okunur.

**F23 tamam** (CHANGELOG "F23"): gerçek saatli testlerin kuralı
CONTRIBUTING "Gerçek saatli testler"de — tick/motor süresi sayan test
paused saatte; duvar saatine bağlı (toplayıcı periyodu, bağlantı
pencereleri, loadgen son tarihleri) test koşulu bekler; sahte uç ölçülen
kodun darboğazı olmaz. (B44, B45, F24 küçük paket 4'te kapandı.)

**B41 tamam** (RECONNECT §16.1, CHANGELOG "B41"): `disconnect` altında
dolu posta kutusunun arkasında bekleyen kapatma isteği, `parked`'ı
gönderimde yeniden sınayarak (`reconcile_closes`) yollanır — parkı önden
biten istek despawn yerleşir.

**B40 tamam** (RECONNECT §16.2, CHANGELOG "B40"): `leave_room` idle-kick'i
bağlantıyı LEAVE sonrası duruma getirir; park `ConnectionId::park_key()`
altında kendi satırına taşınır (`RegistryMsg::LeaveConn`,
`ConnIn::LeftRoom`). B41, E8 ve B43 kendi turlarında, E9 kararla kapandı.

**E6 tamam** (RECONNECT §16.1, CHANGELOG "E6"): AFK'nın odadan mı sunucudan
mı atılacağı oyunun/dağıtımın seçimi — `afk_action` (varsayılan
`leave_room`; `disconnect` bağlantıyı ERROR 9 ile kapatır, `idle_input`
sayılır). Fiil oda→registry→bağlantı (`RegistryMsg::CloseConn`); registry
satırını kendisi yerleştirir; dolu posta kutusunda istek odanın kuyruğunda
kalıp sonraki tick yeniden denenir. E8 kendi turunda kapandı (B40 kapandı).

**E1 + F21 tamam** (SECURITY §3.4, CHANGELOG "E1 + F21"): kova bağlantı
aktöründe (`conn/gate.rs`); sayı odanın config'inden registry join'de
`registry::Seat` ile damgalanır (API: `SpawnPlayer` cevabı artık `Seat`).
Oyun varsayılanı `GameModule::input_rate()`; tek şablon
`Config::room_template_for(...)` — başlangıç, `/rooms/open`,
`ServerHandle::room_config`. Aksiyon kanalı her yerde
`RoomConfig::action_channel()` ile kurulur (0 → 1 yuva). Loadgen teli GSME.

**E2 tamam** (OPS §3/§6, DESIGN §12, CHANGELOG "E2"): dışa açım
`MetricsCollector::emit`'te — `with_exporters` ile kurulan `Exporter`'lar
sırayla, sonra sink. **Yeni bir çekirdek sayacı artık
`metrics/export/families/{room,session,server}.rs`'e bir satır olarak
girer** (Prometheus ve OTLP ikisi birden alır). Prometheus metni değişirse
`metrics::tests::golden` ile `metrics::tests::otlp::golden` birlikte
güncellenir. Feature'lar: `gsb-core` `prometheus` (vars.) / `otlp`;
OTLP'ye dokunan turda `--features gsb-server/otlp` ile de koş.

**B23 tamam** (RPC-CONTROL-PLANE §8.2, CHANGELOG "B23"): `gsb-loadgen
--rpc-rate R [--rpc-burst B]` (demo, düz istemci); istemci defteri
`loadgen/client/rpc/ledger.rs`. Ret nedeni metinleri `gsb_core::rpc`
sabitlerinde — istemci onları okur, kopya yazma.

**B18 tamam** (OPS §2, GAME-MODULE §4.3, CHANGELOG "B18"): `Config::rooms`
(`[rooms.<id>]`, `RoomOverride`) şablonla birlikte `RoomTemplate`'te;
`RoomTemplate::room(id)` tek katmanlama noktası. Yeni bir oda anahtarı
artık üç yere gider: `room_template`'in eşlemesi, `RoomOverride` alanı ve
`RoomOverride::apply`. Başlatma kontrolü `Config::check_room_overrides` →
çekirdeğin `RoomConfig::step_divisor`'ı.

**F5 tamam** (DESIGN §9.2, GAME-MODULE §6 karar 7): bir oyun servisi
`spawn_registry` içinde, registry'den ÖNCE `parts.service(Service)` ile
kaydedilir (`gsb_core::service::Service` = görev + senkron durdurma
isteği; tipik gövde `gsb_core::channel::post(&tx, Stop)`, bant içi).
`stop()` önce odaların düşme bariyerini bekler, sonra servisleri durdurur;
ikisi de 1 sn sınırlı, aşan servis abort ve `StopReport.services_aborted`.
Yeni bir servis durdurma isteğinde yeni iş almamalı, borçlu olduğunu
teslim edip bitmeli (ekonominin `serve`'ü örnek).

**F18 tamam**: `shard_members` N+1 çekirdek hatası değil, yırtık raporun
toplanmasıydı; kural `loadgen/report/spread.rs`'te (`consistent_cut`).
Shard başına sayıların sözleşmesi DESIGN §12 "tutarlı kesit".

**B31 tamam** (DESIGN §6 "El sıkışan kapılar", SECURITY §4.3): WS/TLS/QUIC
kapılarında el sıkışma `gsb_net::transport::intake`'te bağlantı başına
görev; sınır sunucunun unauthed cap'inden (`boot/start/pre_auth.rs`).
`Listener::handshake_stats()` (varsayılan `None`). Yeni bir el sıkışan kapı
el sıkışmayı `accept()` içinde YAPMAZ — intake'i kullanır.

**Küçük paket 3 tamam** (CHANGELOG "Küçük paket 3"): rUDP oturumunu aktör
ölünce yazıcısı bırakır (demux'a kuyruk + kendi adresine uyandırma
datagramı; demux'a ikinci beklenen kaynak EKLEME) (B6). Yeni bir
`Listener` accept'inin tamamını `gsb_net::transport::Door::admit`'ten
geçirmeli ve `close`'ta kapıyı kapatmalı; döngü `is_listener_closed`'da
döner — kapısız listener `stop()`'a 1 sn ve bir abort ekler; `stop()`
`StopReport` döndürür (B16). Tıkalı bağlantının yanıtsız RPC retleri
`requests_refused_congested` (GSMD) (F15). Tick'le karşılaştırılan ya da
paused testte durması gereken her yeni son tarihin İKİ ucu da
`gsb_core::ticker::now()` okur (F16). `counters_dropped` mantık sayacı
adı ayrılmış (F17).

**B29 tamam** (DESIGN §5.7 "Loadgen WS modu", CHANGELOG "B29"):
`gsb-loadgen --transport ws` her modda; WS ↔ TCP taban çizgisi DESIGN
§5.7'de. (B31 kapandı.)

**F9 tamam** (CHANGELOG "F9", DESIGN §12, OPS §3, KIT-ARCHITECTURE §10
"F9"): oyun/kit çekirdeğe dokunmadan kendi adlı kümülatif sayaçlarını
raporluyor — `const LogicCounter` bildirimi, `GameLogic::logic_counters`
(kit: `Game::counters`), oda başına en çok 16 ad. Görünüm: `logic_<ad>=`
(satır ve RESULT), `gsb_room_logic_<ad>_total` / MAX için gauge, loadgen
teli `GSMC`. ÇEKİRDEK sayaçları (ör. F15) hâlâ sabit şekilli örnekte.

**Küçük paket 2 tamam** (CHANGELOG "Küçük paket 2"): WS kapısı sunucunun
bitirdiği her oturumu 1001 ile kapatır ve ilk kapanıştan sonra hiçbir
çerçeve yazmaz (B24). Loadgen churn baytları gerçek wire baytı (B26).
Ticker `tokio::time::Instant` ile damgalıyor; damgayla karşılaştırılan
yeni bir okuma `gsb_core::ticker::now()` kullanmalı, CPU süresi ölçen
kod `std::time::Instant` (F10, TICK-ARCHITECTURE "Tick saati").

**B25 tamam** (DESIGN §5.7, CHANGELOG "B25"): `gsb-client`'in WS yarısı
aynı `Conn::Stream` — yeni varyant yok (loadgen `Conn`/`Recv`'i kapsamlı
eşler). Kapanış `Recv::Closed` + `Conn::ws_close()`; sözleşme dışı çerçeve
`FrameTx::ws_frame`. Yalnız `gsb-net` ws süitinin sahte istemcisi el
yazması kalır (döngü).

**B19 tamam** (DESIGN §5.7, CHANGELOG "B19"): Rust istemcisi artık
`gsb-client` (çerçeve, `Conn`, oturum adımları, tipli `ServerError`).
Kural: yeni bir istemci (loadgen modu, test, örnek) çerçeveyi ya da el
sıkışmayı yeniden yazmaz — `gsb_client::{connect, tls, quic, session}`
kullanır; `timeout` ile sarılan okuma `Conn::recv` / `FrameRx::next`
(iptal-güvenli) olur, `read_exact` değil. WS istemcisi de `gsb-client`'te
(B25); `gsb-net` `gsb-client`'e bağımlı olamaz (döngü). Gerçek-sunucu
testleri `gsb-server/tests/client_session.rs`'te.

**A29 tamam** (KIT-ARCHITECTURE §10 "A29", CROSS-SHARD §8b.1): takım
bütçesi kestiğinde kademe içinde ne kalacağını oyun seçer —
`ShardedTeamRoom::with_export_rank(fn(&Wire<G>) -> u32)`, önce üyeler
korunur, eşitlikte küçük wire id; sıralamasız kesme A29 öncesiyle bayt
bayt aynı (sabitli). Kesme `TeamExport::over_budget` →
`RoomSample::team_over_budget` / `gsb_room_team_over_budget_total` /
loadgen `GSMB`.

**B12/B13 tamam** (DESIGN §5.6, CHANGELOG "B12 + B13"): `stop()` ERROR 14,
`stream_rejected` ERROR 9 `stream rejected: …` gönderir — ikisi de
beklemesiz `try_send` (`conn/actor/close.rs::try_notice`). Kural: yeni
bir kapanış bildirimi istemciye bağlı await EKLEMEZ; `Listener::close`
canlı oturumları kısa kesmez (QUIC `set_server_config(None)`); WS'te
kapanış çerçevesinden sonra veri çerçevesi yok (soket yazıcı görevi).

**F14 tamam** (RPC-CONTROL-PLANE §3.1, CHANGELOG "F14"): düşen batch'in
RPC yanıtları çekirdekte `queued`'ın başına dönüyor (oda 4d / shard 6d)
ve sonraki kabul edilen batch'le tam bir kez gidiyor. Tıkalı bağlantı
(`dropping`) borcu `max_pending_requests_per_conn`'a ulaşınca
`refuses_congested` yeni isteği 2a/2c'de yanıtsız reddediyor; sınır cap +
tick çekimi. Mantık için değişen yok: `private` verilen `responses`'ı her
tick kodlamalı (zaten öyle).

**F11 tamam** (KIT-ARCHITECTURE §10 "F11", CHANGELOG "F11"): `GameLogic`
iki no-op kanca kazandı — `on_batch_dropped(world, player, snapshot)` ve
`on_batch_resumed(world, player)`; oda/shard fan-out'unda o oyuncunun
`private`'ının hemen ardından çağrılıyor. "Gönderdim" diye tek seferlik
durum tutan yeni bir mantık onu burada yeniden kurmalı (kit: `InputSeq`
taşınan yuvası, `Baselines::dropped/resumed`). Yavaş okuyucu ölçümü:
`gsb-loadgen … --stall-ms 10000 --stall-every-ms 15000 --conn-out 4
--write-stall-secs 0 --capture DIR`. (F14 kapandı.)

**F8 tamam** (OPS §2, RECONNECT §17, CHANGELOG "F8"): oda config'inin
tek kaynağı `Config::room_template` (`config/axes/listeners/room.rs`);
ops yüzeyi `OpsHttp.room_template` tutuyor. Yeni bir oda anahtarı yalnız
`room_template`'e eşlenir — iki yol da alır. Programatik yol:
`ServerHandle::open_room(cfg.room_config(id))`.

**A10 tamam** (KIT-ARCHITECTURE §10 "A10", CHANGELOG "A10"): seam
`RecordCodec::send_every(&Wire) -> SendEvery`, takvim `SendEvery::due`.
`SetLedger` vadesizde `held`'i ilerletmez; `CellBook` bekleyenleri
`roll` başında salar; sharded takım ihracı delta modunda yalnız vadede
ilerler. Kilitler: `record_run` dokuz oturum özeti (varsayılan bayt),
`record_run::rate` oranlı ikizler (sınır tam erişilir, hiç aşılmaz).

**A31 tamam** (KIT-ARCHITECTURE §10 "A31", CHANGELOG "A31"): kayıt
çerçevesi oyunun seçimi (`RecordCodec::RUN` / `ClientDecoder::RUN` +
`run_record`); açmayan oyunun baytı değişmez. Shard'lı bir odanın
karelerini bayt bayt karşılaştıran test gerçek aktörlerle DEĞİL elle
adımlanarak yazılmalı (A31-1, kit `record_run/shards`).

**A30 tamam** (KIT-ARCHITECTURE §10 "A30", CHANGELOG "A30"): wire id'ler
iç içe basılıyor; formül çekirdekte tek yerde
(`gsb_core::shard::{interleaved_id, minting_shard, SHARD_SERIAL_CAPACITY}`),
kit'in `Minter::Interleaved`'i onu çağırıyor; `ShardLogic` artık
`serial_capacity` + `serial_used` istiyor (`serial_base` yok). Kendi
id'sini basan yeni bir `ShardLogic` taslağı `interleaved_id` kullanmalı.

**Motor, oyun değil (kullanıcı kuralı):** kit/çekirdek yapı taşı sunar;
kayıt formatı, zarf sürümlemesi, gönderim hızı, interpolasyon oyunun
kararı — yeni seam'ler opt-in, varsayılan bugünkü baytlar; demolar yalnız
bir seam'i doğrulayacak kadar dokunulur.

**A22 faz 0 tamam** (KIT-ARCHITECTURE §10 "A22", CHANGELOG "A22 faz
0"): kodda yalnız `gsb-loadgen --capture`; bölüm ölçümleri ve öneriyi
içeriyor. Sıradaki iş bakımcının beş cevabına bağlı (BACKLOG E7): kayıt
gövdesi protobuf'tan çıkabilir mi, zarfın yeni alanı nasıl sürümlenir,
wire id basımı değişebilir mi, A10/Unity interpolasyon önceliği, A22
tetikleyicisi. Uygulama turları `--capture` + `ClientView` ile ölçer.

**W2 tamam** (KIT-ARCHITECTURE §10 "W2 sonucu", GAME-MODULE "W2 sonucu",
CROSS-SHARD §8b.8): "Cephe" (`gsb-demo-war`) kompoziti kit'e dokunmadan
kullanıyor; `game = "war"` (`[war]`: `disconnect_grace_secs`,
`team_budget`), `--game war` (kadro `lg-{id}`, RESULT'ta `team_*`).
Takım sayaçları `RoomSample::team_*` (loadgen teli `GSMB` — A29'da `team_over_budget` eklendi). Aktör testleri
public API'den canlı registry + dört shard'la (`gsb-demo-war/tests/common`);
yürüyen senaryo da duraklatılmış saatte (F10: ticker runtime saatinde damgalıyor).
Röle 1000'de kayıpsız; bant istemci tarafında (~365 KB/sn/istemci) —
A10/A22'nin kanıtı.

**W1 tamam** (CROSS-SHARD §8b, KIT-ARCHITECTURE §10 "W1 sonucu"): bir
shard logic'i `ShardLogic::team_exchange` ile takım görünür kümesini
export eder, registry hub'ı onu görüntüleyen shard'lara röle eder. Takım
odası kimliği `TeamGame::spawn_team_player_as`'a iletir. Gerçek aktör
testleri duraklatılmış saatte (kit dev-dep `tokio` `test-util`); kalıbı
`sharded/tests/team_actors/rig.rs`.

**T tamam** (KIT-ARCHITECTURE §10 "T sonucu", CHANGELOG "T turu"): bir
küme-içerikli oda delta'yı ortak defterden alır (`common::SetLedger` +
`Baselines` + `emit_private_full`); takım odasında `with_delta` ile
açılır, arena açık. Hareketli yükte kazanç ~%10 — daha fazlası değer
düzeyinde delta (A22).

**S tamam** (DESIGN §9.1, CHANGELOG "S turu"): `stop()` artık her zaman
bitiyor. Registry'nin durdurma yolları (`on_shutdown`, `on_destroy_room`)
senkron; oda/shard'a Shutdown `registry::actor::stop::post_stop` ile
gider (yer varsa yerinde, doluysa spawn'lu gönderici). Kural: registry
bir odanın posta kutusunu satır içinde asla beklemez — registry'nin tek
await'i kendi posta kutusu.

**H tamam** (DESIGN §6 "El sıkışma kaybı", SECURITY §4.2, CHANGELOG
"H turu"): rUDP `connect` sunucunun kabulünü (`ACK{1}`) bekliyor, kayıp
proof/challenge/kabul yeniden gönderimle iyileşiyor, 5 sn'de temiz
`TimedOut`. Loadgen rUDP ölçümlerinde artık `--stagger-ms` gerekmiyor;
el sıkışma kaybı `hs_retries` ile görünür.

**Küçük paket tamam** (CHANGELOG "Küçük paket turu"). Kendi özel yükü
olan bir oyun `Game::session_private`'ı ezer: kit onu oturum başına bir
kez (join ve resume sonrası ilk private karede, göçte değil) sorar, alan
4'e ekler; istemci `ClientDecoder::session_private` ile alır, yalnız bu
yükü taşıyan kare `PrivateEvent::Session`'dır. Yeni bir oda-seviyesi
config anahtarı `Config::room_config`'e yazılır. Rapora yeni bir sayaç
eklerken iki aktörün `sample()`'ı, `RoomReport`, toplayıcı, `render`,
Prometheus tablosu, loadgen codec'i (magic'i artır) ve `fold_rooms`
birlikte değişir. rustdoc artık CI kapısı.

**U tamam** (DESIGN §6 "MTU", SECURITY §4.1, CHANGELOG "U turu"): rUDP
oyun bandında bütçeyi aşan kare FRAG datagram'larıyla gidiyor, istemci
birleştiriyor (16 parça, 4 slot, 64 KiB, 250 ms); kit/çekirdek zarfı
değişmedi.

**K4 tamam** (GAME-MODULE "K4 sonucu", CHANGELOG "K4 turu"): sharded
yönlendirici `(conn, kimlik)` alıyor (`gsb_core::registry::HomeShard`),
taze join `GameLogic::on_join_as`'tan geçiyor, kit `Game::spawn_player_as`'ı
çağırıyor, MMO `Realm::logins` kimlikle anahtarlı. Ticket'sız yolda
kimlik = iddia edilen `Auth.name` → yalnız geliştirme yolu (SECURITY
§4b). Loadgen MMO'yu bot kadrosuyla barındırıyor (`bot/mmo/roster.rs`).
Stok `gsb-server` MMO'sunda kayıtlı karakter yok (kalıcılık gelene dek
herkes waystone 0'da).

**D tamam** (CROSS-SHARD §4d, CHANGELOG "D turu"): `h`'de devredilen
entity'nin kopyası `h + 1`'de eski dünyada durur ama oyunun kancaları onu
GÖRMEZ (`Disabled`) — `Seam` onu yeni sahibin ödünç kaydı olarak verir,
`emit` oraya gider. Bir hedefi wire'dan çöz (dünya sorgusu ya da
`Seam::local`); saklanmış bir `Entity` tutamağıyla doğrudan yazma
`Disabled`'ı atlar ve kopyaya iner. Kit'in kendi geçişleri kopyayı
sistemlerden sonra eskisi gibi görür.

**Cross-seam C2 tamam** (CROSS-SHARD §4c, CHANGELOG "Cross-seam C2
turu"): opt-in `with_crystallize(Crystallize)` ile seam ötesi süren
dövüş tek shard'a taşınır ve orada TUTULUR — `collect_migrations` önce
pin'in anchor'ına, yoksa `region_of`'a bakar. Crystallization'ı açan
bir oyun KENDİ ENTITY'SİNE indirdiği yerel darbeyi `Seam::contact(source,
target)` ile bildirmeli; yoksa tutulan dövüş `release` sonra sessiz
sayılıp bırakılır. Yeni bir `Partition` önayarı yazarken `holds`'u uygula
(varsayılan her yerde tutar — bant yok); bandı ödünç şeridin
genişliğiyle sınırla.

**Cross-seam C1 tamam** (CROSS-SHARD §4b, CHANGELOG "Cross-seam C1
turu"): sharded bir oyun seam'in ötesini `ShardGame::{ingest_seam,
systems_seam}`'in `Seam`'i ile görür (`lent`/`lent_iter` ödünç kayıt,
`local` kendi entity'si — own wins) ve yabancı entity'yi YALNIZ
`Seam::emit` ile etkiler; otorite `apply_remote_effect`'te uygular.
Yeni bir etki türü eklerken: doğrulamayı saldıranın shard'ında yap,
yükü oyunun baytları olarak kodla, otoritede bayatlığı
(`tick - effect.at_tick`) ve menzili politika olarak yeniden denetle.
Çekirdeğin sabitleri (`EFFECT_*`) doğruluk parametresidir — dedup
penceresinin sınırı bütçe ve yaş tavanından türetildi; birini
değiştirirsen `EFFECT_WINDOW` kanıtını yeniden kur.

**Oyun modülü G3 tamam** (`docs/GAME-MODULE.md` §5 "G3 sonucu"):
loadgen'in oyuna özgü yarısı `loadgen/bot/`'ta — yeni bir oyun için bir
`LoadBot` + `BotClient` yaz (görünüm = kit'in `ClientView<D>`'si; kaydı
`client::wire::Fields` ile elle yürü ve üretilmiş çözücüye testle
sabitle), `bot::games()`/`bot_for`'a ekle. Döngüye (`client/view/run.rs`)
oyun kodu sokma. Oyuna özgü bir loadgen bayrağı eklersen `bot/flags.rs`'e
yaz (başka oyunda hata) ve orkestratör çocuklarına yalnız o oyunda ilet
(`child_args.rs`). RESULT'ta `game=` son anahtar kalır; oyun ekleri
ondan hemen önce. MMO yükü botun ilk `Travel`'ına dayanıyor (K4).

**Oyun modülü G4 tamam** (`docs/GAME-MODULE.md` §5 "G4 sonucu"): kit
zarfının istemci yarısı `gsb_kit::client`'ta. Bir istemci (loadgen botu,
test istemcisi) kendi bayat/delta/private mantığını YAZMAZ:
`ClientView::new(decoder)` + `apply_snapshot(bytes)` /
`apply_private(bytes)`; oyun yalnız `ClientDecoder`'ı verir (kayıt →
`(id, Record)`, `cell_of`, çıkış → hücre — `CellExit` hücrenin
İNDİSİdir, konum değil; G4-1 buydu). Sıcak döngüde kaydı
`client::wire::Fields` ile elle yürü; pinlemek için üretilmiş çözücüyle
karşılaştıran test yaz (loadgen'in `view/tests.rs`'i örnek).

**Kit düzeltme turu tamam** (`docs/GAME-MODULE.md` §5 "Kit düzeltme
turu", CHANGELOG): sharded odalarda oyuncunun girdi oturumu (`InputSeq`:
`hwm`, `acked`) göçle birlikte `KitMig.input` (`ShardInputRecord`) içinde
taşınıyor — kaynak `collect_migrations`'ta OKUR, `on_migrate_out`'ta
siler, hedef `on_migrate_in`'de kurar (`InputSeq::adopt`). Yeni bir kit
kaydı `KitMig`'e girerse aynı deseni izle: toplarken okuma, taşıma
kesinleşince silme (reddedilen gönderimde oyuncu kaynakta kalır).
`KitMig` bugün serileştirilmiyor; Ipc/Net linkinin codec'i her alanı
kapsamalı. `config.example.toml`'da demo anahtarları yorumda: yeni bir
düz demo anahtarı da yorumda yazılmalı (`example_config` kilitliyor).

**Oyun modülü G2 tamam** (`docs/GAME-MODULE.md` §5 "G2 sonucu",
CHANGELOG "Oyun modülü G2 turu"): `game = "arena"` ve `game = "mmo"`
gerçek sunucuda; ayarları yalnız kendi tablolarında (`[arena]`: `teams`,
`disconnect_grace_secs`; `[mmo]`: `logout_grace_secs`, `logout`), sabit
tuttukları düz anahtar ya da tablolarında bilinmeyen anahtar başlatmada
hata. Okuyucular `games::settings`'te public. MMO'nun her oda kimliği
bütün bir shard grubu; kaydı olmayan oturum waystone 0'a (shard 0).
Uçtan uca test yazarken `tests/hosted/`'ı kullan; gömülü realm için
`MmoModule::with_realm` + `start_game_server`. K4 (kayıtlı karakter
oturuma bağlı) bilinen sınır; K1–K3 ve K5 sonraki turda kapandı.

**Oyun modülü G1 tamam** (`docs/GAME-MODULE.md` §5 "G1 sonucu",
CHANGELOG "Oyun modülü G1 turu"): sunucu oyunu `GameModule`
(`gsb-server/src/game.rs`) arkasında barındırıyor; oyuna özgü her şey
`src/games/<ad>/` içinde, her oyun bir cargo özelliği (`game-demo`
varsayılan açık). Config'in `game` anahtarı derlenmiş oyunlardan birini
seçer; üçüncü taraf `start_game_server(module, cfg)` kullanır.
`cargo build -p gsb-server --lib --no-default-features` bir CI kapısı —
`games/` dışına oyun tipi sızdırırsan kırılır. Registry'yi modül başlatır
(`RegistryParts::spawn`, tek generic metot). Demo'nun düz config
anahtarları ve `Config::resolve_selection` uyumluluk katmanı; eksen
hataları `ServerError`'da kaldı. Ekonomi servisi artık altı demo odasının
hepsine bağlı. Yeni oyunların ayarları kendi adını taşıyan tabloda
(`[arena]`, `[mmo]`).

**Küçük düzeltme paketi** (CHANGELOG): Faz 5'in son yan gözlemi kapandı —
`ShardedSpatialRoom` ödünç şeridi `Partition::admits` ile süzüyor
(`sharded/tests/frame_filter.rs`); aşırı yükteki `outbound_dead` yanlış
atfı, writer pump'ın doğumda ayırdığı mailbox slotuyla kapandı
(SECURITY §3.5).

**Süreli bekletmede veto + veto tavanı** (RECONNECT §17, CHANGELOG):
`GameLogic::may_release` süreli bekletmenin deadline'ında da soruluyor
(veto uzatır, her tick yeniden sorulur); duran veto
`RoomConfig::max_detach_hold` (varsayılan 10 dk, DETACH'tan; `None` =
tavan yok, `Some(ZERO)` = uzatma yok) ile sınırlı — süresiz bekletme
dahil. Tavan yalnız vetoyu ezer, grace'i kısaltmaz; tek kod yolu iki
aktör için `room/counters/hold.rs`. Kit kodu değişmedi. MMO: `InCombat`
+ `MmoGame::may_release` (çıkış savaş bitene dek bekler). Test fikstürü
yazarken: `may_release` varsayılanını `true` tut — `false`, her süreli
park'ı veto eder.

**Faz 5 tamam — kit düzeltme turu** (KIT-ARCHITECTURE §10 "Faz 5
sonucu", CHANGELOG "gsb-kit Faz 5 turu"): iki demonun bulgularının
hepsi kit'te kapandı, her biri kendi commit'inde, önce kırılan
testiyle. F1: sharded × spatial'da ödünç hücresine göç eden entity
artık yeni shard'ının kovasından silinmiyor (`integrate_borrowed`,
`sharded/tests/lent_arrival.rs`). F2: `GridPartition2::with_diagonals()`
(8-komşuluk; varsayılan 4). F3: `Planar`'ın birim sözleşmesi — konum ve
wire izdüşümü AYNI birimde; `Partition::debug_check_wire` debug'da
denetliyor. F4: her odada `with_disconnect_policy(grace, to)` +
`Game::may_release` (varsayılan politika aynı). A1:
`TeamGame::spawn_team_player` (takıma bağlı spawn). A2: istemci
kuralları `kit.proto`'da. A3: **`Game::SNAPSHOT_OP` / `PRIVATE_OP`
varsayılansız** — yeni bir oyun kendi opcode bloğunu seçmek zorunda.
`gsb-core` dokunulmadı, baytlar aynı; kit tasarımının kabul kriteri
(§11) tamamlandı. Kit'e yeni bir şey eklerken: altı odanın hepsinde
aynı yüzeyi koru (politika / veto testleri altısını birden sürüyor:
`common/park/tests.rs`).

## ÖNCE OKU (sırayla)

1. `README.md`
2. `docs/ROADMAP.md` — başındaki **DEVAM NOTU** zorunludur (ajan kaybı
   dersleri + ortam notları).
3. Aşağıdaki iş sırasına göre ilgili turun sözleşme dokümanı.

## ZORUNLU DİSİPLİN (istisnasız)

- `crates/gsb-lint` build'i kırar: kaynakta `tokio::select`,
  `futures::select`, `select!`, `Mutex`, `RwLock`, `parking_lot`
  geçMESİN (yorumlar sökülür ama STRING LITERAL KORUNUR — bu kelimeleri
  string'e bile yazma!).
- Aktörler tek-awaited; kanallar bounded; hot path `try_send/try_recv`.
- Çalışma döngüsü: iddiayı doğrula → düzelt → davranış-kilitleyici test
  (mümkünse mutation-check) → clippy 0 uyarı → tüm süit yeşil → kommit.
- Yeni bağımlılık gerekiyorsa cargo komutlarının başına
  `CARGO_HOME=$PWD/.cargo` ekle (HOME önbelleği salt-okunur olabilir).
- **Ajan aktifken asla `git add -A`** — yalnız açık pathspec; ya da ajan
  bitene kadar bekle. (Bu oturumda iki kez pahalıya patladı.)
- Paralel ajan çalıştıracaksan `git worktree` kullan (aynı ağaçta iki
  ajan = dosya çakışması; bu oturumda bir kez oldu).
- Her turda: ROADMAP durum satırı güncellenir, elenen alternatifler
  belgelenir, rapor iddiaları parent tarafından koddan doğrulanır.
- Doküman geleneği: tasarım kararı vermeden önce ELENEN ALTERNATİFLER
  yazılır; tetikleyicisiz optimizasyon yapılmaz ("önce veri").

## SABİT MİMARİ KARARLAR (yeniden tartışma — hepsi ölçüm/kararla sabit)

- rUDP deneysel statüde; REL/RAW band kuralı: opcode ≤64 güvenilir,
  ≥1000 kayıp-toleranslı. Band seçimi BOYUT değil KAYBIN BEDELİ
  (DESIGN §5.1).
- Delta border exchange main'de (A/B: 0.39× byte); Faz C sonrası
  aynı-process komşular AlwaysFull (lokal CPU kıt).
- Kalıcılık iki sınıf: maç oyunları = her-tick typed CLONE checkpoint
  (encode ASLA — checkpoint process'i terk etmez); MMO = event-based +
  periodic checkpoint + logout flush; otorite merkezi katmanda, game
  server cache'tir. Detay: `docs/PERSISTENCE.md`.
- Üç eksenli config: `topology × visibility × communication`;
  desteklenmeyen kombinolar startup'ta faz-bilgili hata ile reddedilir.
- `DemoRoom` artık `OpenRoom`; `BorrowedRecord` artık
  `BorderRecord<Strip>` (core zarf + logic payload); shard↔komşu
  haberleşmesi `ShardLink` trait'i arkasında.

## İŞ SIRASI (sözleşmeli turlar)

1. ~~**Yayın paketi**~~ — **KAPANDI** (CHANGELOG "yayın paketi turu"):
   MIT `LICENSE`, MSRV = sabit toolchain = 1.95.0 (alt sınır
   `bevy_ecs 0.19.1`), `.github/workflows/ci.yml` (fmt check · clippy
   `-D warnings` · test), `CONTRIBUTING.md`. (Build artık sistem
   `protoc`'u İSTEMİYOR — protokol sertleştirme turu gömülü protoc'u
   bağladı, CI'daki `protobuf-compiler` adımları kaldırıldı.)
   `cargo fmt --all --check` temiz — artık CI kapısı; yeni kod
   formatlanmış gelmeli.
2. ~~**Protokol sertleştirme**~~ — **KAPANDI** (CHANGELOG "protokol
   sertleştirme turu"): RPC yanıt zarfı base'e taşındı (DESIGN §5.2),
   `reserved` disiplini kuruldu (§5.3), ERROR kodları üretilen enum
   oldu (§5.4) ve protokol sürümü AUTH'a eklendi (§5.5). Tel
   değişmedi. Yeni kod yazarken: `ERROR` üretmenin tek yolu
   `base::Error::new(ErrorCode::…, msg)`; yeni bir `CoreError`/
   `ProtoError` varyantı eklemek `wire_code` eşlemesini DERLEMEZ hale
   getirir (kasıtlı); `.proto`'dan alan silersen aynı commit'te
   numarasını VE adını `reserved` et, commit'i gerekçe olarak an.
   Açık kalan tek parça sürüm ARALIĞI politikası — tetikleyicisi
   ROADMAP'te ("Protokol sözleşmesi" bölümü).
3. ~~**WS uyum kapısı**~~ — **KOD TARAFI KAPANDI** (CHANGELOG "WS uyum
   kapısı turu", SECURITY §3.7): §5.4 araya girme guard'ı (1002), §5.2
   uzunluk kuralları, §7.4/§8.1 kapanış doğrulaması; her kural okuyucu
   seviyesinde testli. CI'daki `autobahn` işi (`examples/ws_autobahn` +
   `.github/autobahn/autobahn.py`) yerelde gerçek imajla koşuldu
   (`crossbario/autobahn-testsuite:25.10.1`): 98 vaka, 96 OK + 2
   INFORMATIONAL, denetleyici geçti, `ACCEPTED` boş. İlk yazılan
   `0.8.2` etiketi Docker Hub'da yoktu; düzeltildi. Kalan tek şey:
   repo push edildiğinde CI'daki ilk koşunun raporuna bakmak.
4. **team × sharded export** — sözleşme: `docs/CROSS-SHARD.md §8`
   (registry-hub BYTE-ENCODED takım-export; RegistryMsg monomorfik
   kalır — generic'e çevirme ELENDİ; TTL sweep + fan-out + izolasyon
   kuralları dahil).
5. ~~**Cross-seam etkileşim paketi**~~ — KAPANDI (C1: borrowed-view
   erişimi, `ShardMsg::RemoteEffect`; C2: histeresizli
   crystallization). Yan bulgu (`snap_overflows`) U turunda rUDP
   parçalamayla kapandı.
6. Tetikleyicili bekleyenler: NUMA ölçümü (numactl pinli/pinsiz),
   ortak DeltaSnapshotCodec adoptasyonu (all/pvs — team T'de yapıldı), Ipc/NetLink,
   QUIC rehome.

## BEKLEYEN KULLANICI KARARLARI (kendine sor, tek başına verme)

- `metrics` crate fasadına geçiş mi elle render mı (OPS §6 kenar notu).

## DOĞRULAMA TABAN ÇİZGİSİ

Her turdan sonra: `cargo fmt --all --check` → temiz;
`CARGO_HOME=$PWD/.cargo cargo clippy --workspace
--all-targets -- -D warnings` → 0 uyarı; `CARGO_HOME=$PWD/.cargo cargo test
--workspace` → tamamen yeşil (bugün itibarıyla 1811 passed, 2 ignored);
`cargo run --release -p gsb-server --bin gsb-loadgen -- 50 --duration 3`
→ left=50, errors=0, panic yok.
