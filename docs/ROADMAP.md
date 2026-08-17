# Ne kaldı? — gsb Geliştirme Listesi

Durum: v1 mimari tamam; dış incelemede bulunan 5 kritik hata (#1–#5)
kapatıldı; tick mimarisi broadcast tabanlı yeniden kuruldu (ayrı
`docs/TICK-ARCHITECTURE.md`); yayın fazı **grup başına tam dünya
snapshot'ı** modeline geçirildi (aşağıda); grup adilliği sözleşmesi
(grup başına defter) + F1/F3/F4 kapatıldı (aşağıda); `3f25874`'ün
kendi denetim raporundaki 5 bulgu (Bulgu 1–5) kapatıldı (aşağıda).
30/30 test yeşil. Aşağıdakiler **ölçülmemiş performans**,
**robustluk** ve **güvenlik** başlıklarındaki kalan işler.

## Kapatılanlar (denetim turu — `3f25874` denetimi)

- [x] **Bulgu 1: `late_joiner_receives_full_world_snapshot`'ın
  ConnectionId / `spawn_pos` / `DEFAULT_SPEED` / tick periyodu
  bağımlılığı** — F1 sonrası emisyon wire içeriğiyle (tam sayı
  konum) kapalıyken commit'teki loop'un "tek tick besle → batch'i
  bekle" (bloklama) yapısı, yalnızca ilk hareket tick'inde ızgara
  sınırının geçildiği id'lerde geçiyordu: 199 id taramasında 91'i
  k=1 (geçer), 108'i k>1 (5 sn bloke → `timed out waiting for a
  batch` panic). Düzeltme: `stale_leave` testindeki desene çekildi —
  tick besle, `sleep(10 ms)`, batch **hangi tick'te gelirse**
  `try_recv` ile al; 60 tick bütçesi ve tüm assertler korundu. Kabul
  ölçütü: aynı 199 id'lik oda-seviye tarama düzeltme sonrası
  199/199 geçti (probe commit edilmedi; ham çıktılar raporda).
- [x] **Bulgu 2: `EntityVersion` okuyucusuz kaldı, dokümanlar onu
  hâlâ kullanımda anlatıyordu** — F1 kararı wire içeriğine
  taşıyınca versiyonun tek okuyucusu gitti; yazanlar (spawn'da
  insert, MOVE_TO ingest + her hareket tick'inde `bump`) ve sistem
  (`gsb-ecs::dirty`, `systems.rs`, DESIGN §7, ROADMAP bu maddenin
  atıfı, README crate tablosu) hâlâ aktif mekanizma gibi duruyordu.
  Düzeltme: mekanizma **kaldırıldı** (alternatif "sakla + dokümanları
  düzelt" reddedildi: okuyucusuz yazım hot path'te bedava değil,
  "bump disiplini" artık hiçbir şeyi zorlamaz; gerekirse P2 delta
  yayınında o özellikte yeniden getirilir). `gsb-ecs::dirty` modülü,
  prelude re-export'u, 3 çağrı noktası ve `frame_independence`
  spawn tuple'ı temizlendi; 2 unit test uyarlandı (amaç/assertler
  korundu), biri yeniden adlandırıldı
  (`snapshot_emits_on_plain_position_write`).
- [x] **Bulgu 3: F4 tanısı, motivasyon olan 1/26 aç kalma modunda
  tetiklenmiyordu ve testsizdi** — Ön koşul ölçümü (probe; ham çıktı
  raporda): paylaşımlı defterli mantıkla 16 (A,B) çifti, her iki
  ziyaret sırası da oluştu. Erken girenin aç kaldığı 7 çift =
  ölçülen 1/26 modu (A=1, B=26): **0 uyarı** → ana iddia
  **doğrulandı** (kaybeden grup kendi join tick'inde yayın yapmış →
  `last` dolu → koşul asla tetiklenemez). Geç girenin aç kaldığı 9
  çiftte tick 3'te tam 1 uyarı, grup adıyla. Düzeltme: (1) yeni
  `RoomActor` regresyon testi — asla yayın yapmayan `RoomLogic`
  stub'u + kilsiz (mpsc tabanlı) `tracing` subscriber'ı: uyarı
  **tam bir kez**, grup adını söyleyerek tetiklenir
  (`never_emitted_group_warns_once_naming_the_group`); (2) kapsam
  netleştirildi (DESIGN §4 Tanı maddesi): kaybedeni en az bir kez
  yayınlamış paylaşımlı-defter aç kalması ve tek tick ihlal eden
  grup (join+leave aynı tick'te; `gone` budaması) yakalanamaz — oda
  meşru sessizliği ayırt edemez, koruma sözleşme metnidir. Yan
  etkiler: uyarı metnindeki "(and every tick since)" kehaneti
  düzeltildi; log alanı için gereksiz `key().clone()` kaldırıldı.
- [x] **Bulgu 4: ROADMAP F2 maddesi güncel değildi** — F1 turu
  eklenen 2 unit test DemoRoom "değişiklik yok" yolunu doğrudan
  kapsarken madde hâlâ "P1 olarak duruyor" derdi. Düzeltme: madde
  kapatıldı ve testlere atıf yapıldı (aşağıda, grup adilliği turu
  bölümü); odaseviye sessizlik zaten
  `unchanged_group_is_silent_until_keepalive`'da.
- [x] **Bulgu 5: DESIGN §11'deki "regresyon" iddiası yanlıştı** —
  yeni core testi paylaşımlı defter yanlış kullanımını yakalayamaz
  (test mantığı grup başına defterli çalışır; oda meşru sessizliği
  ihlalden ayırt edemez). Cümle düzeltildi: test, oda tarafının
  grup-başına davranışına regresyondur; mantık tarafı koruması
  sözleşme metnidir.
- [x] **Diğer doküman hataları (denetimde ayrıca bulundu, aynı turda
  düzeltildi):** README — BROADCAST fazı açıklaması (eski "dirty
  entity → batch + flush"), opcode listesi (eski
  `ENTITY_SPAWNED/REMOVED/STATE`), test sayısı; DESIGN — "önbellek
  hiç dolmadıysa hiç nothing" (karışık dil); gsb-core manifest
  açıklaması ("4-phase tick" → 5 faz).

## Kapatılanlar (grup adilliği turu)

- [x] **Grup başına defter sözleşmesi (per-group state contract)** — dış
  ölçüm: DemoRoom defteri 1:1 kopyalanıp `type GroupKey = ConnectionId`
  yapıldığında (arayüzün reklam ettiği kullanım) 2 bağlantı + her tick
  hareket eden 1 entity + 25 tick'te dağılım **1/26**'ydı (kaybeden
  bağlantı kendi join tick'inden beri yeni snapshot alamıyordu; dağılım
  çalıştırma içi 100/0 — group tablosu `HashMap`'inin ziyaret sırası
  koşu boyunca sabit; kazananın kimliği process bazında
  `RandomState`'ten). Mekanizma: oda her tick'te her grup için
  `snapshot()`'ı birer kez çağırıyor (4c, `groups.iter_mut()`); paylaşımlı
  `last` defteri tek adettediğinden ÖNCE ziyaret edilen grup değişikliği
  tüketip defteri yazıyor, SONRAKİ grup aynı tick'te "değişiklik yok"
  görüyor. Düzeltme katmanı: oda tarafında mekanik koruma imkânsız
  (mantık durumu opak; sessizlik meşru bir hâl) → (1) `RoomLogic::snapshot`
  sözleşmesine açık şart eklendi: karar + defter `group` anahtarıyla
  tutulmalı, çağrı sırası belirsizdir, tek defter yalnız `GroupKey = ()`
  odalarda doğrudur; (2) demo modül/alan dokümanına aynı uyarı; (3) core
  regresyon testi: her tick değişen dünyada aynı tick'te değişen **her**
  grup yayınlanır (grup başına defterli mantıkla, 27/26 payload
  dizileriyle doğrulandı); (4) `GroupKey`'ye `Debug` bound'u.
- [x] **F4: asla yayın yapmamış grup için tanı** — üyesi varken hâlâ hiç
  snapshot üretmemiş grup (ilk tick'te `snapshot` → `false`; üyelik
  değişikliği bir değişiklik olduğundan sözleşme ihlali) oda tarafından
  **bir kez** `warn!` ile loglanır (grup adı + üye sayısı). Sessiz
  (en az bir kez yayınlamış) gruplar tetikleyemez → meşru AOI sessizliğinde
  yanlış alarm yok.
- [x] **F1: demo "değişiklik yok" kararlayıcısı wire içeriğiyle** —
  `last` defteri `(entity → (x, y))` (wire'ın kesilmiş konumları) oldu.
  `bump()` disiplini (önce yalnız yorumdaydı) artık yayın kararını
  etkilemez: `bump()`'sız `Position` yazımı yayınlanır, içeriği
  değiştirmeyen `bump()` yayınlatmaz (ROADMAP'teki "bump() her tick aynı
  tam sayıyı yayınlar — bant israfı" noktası bu tur kısmen kapatıldı;
  kalan parça koordinat formatı kararı). 2 unit test eklendi.
- [x] **F3: `GroupState::members` ölü alanı kaldırıldı** — her
  tick/grup boşa `Vec` alloke edip hiçbir okuma yoktu; fan-out zaten
  bağlantı tablosundan çalışıyor. Grup başına durum artık
  {son snapshot, bu tick'in gönderimi, tanı bayrağı}.
- [x] **F2 (DemoRoom no-change yolunun testsizliği) — denetim turunda
  kapatıldı:** yol artık doğrudan testli — `snapshot_emits_on_plain_
  position_write` (düz `Position` yazımı yayınlanır) + `snapshot_
  silent_when_wire_content_unchanged` (içeriği değiştirmeyen yazım
  sessiz, leave yayınlatır); odaseviye sessizlik + keepalive yeniden
  gönderimi ayrıca `unchanged_group_is_silent_until_keepalive`'da.
- [ ] F5 (keepalive ≥ tick hızı) ve F6 (21→16 B wire iddiası) denetim
  turunun kapsamı dışında — sırasıyla P1 ve doküman düzeltmesi olarak
  duruyor.

## Kapatılanlar (snapshot turu)

- [x] **Yayın: grup başına tam dünya snapshot'ı** — bağlantı başına
  `last_sent` defteri + `pending_out` + `ENTITY_SPAWNED`/`ENTITY_REMOVED`
  kaldırıldı. `RoomLogic`'e `GroupKey` (Eq+Hash+Clone) + `group_of()` +
  `snapshot()` (false = değişiklik yok; üyelik değişimi dahil) +
  `private()` eklendi (demo `GroupKey = ()`; arayüz `ConnectionId` gibi
  oyuncu başına grubu da destekliyor — core'de testle doğrulandı). Oda,
  tick başına grup başına snapshot'ı BİR KEZ kodlar, `freeze()`'ler ve
  `Bytes` (Arc) klonu olarak dağıtır; `OutSink`/kare tamponlaması kalktı.
  Proto: tekil `EntityState` → paketlenmiş `WorldSnapshot` (entity
  kayıtları + monoton `sequence`); kayıt başına versiyon alanı düştü
  (21→16 B) — sıralamayı `sequence` üstleniyor. Değişiklik yoksa yayın
  durur; yapılandırılabilir keepalive (varsayılan 1 Hz, `keepalive_hz`)
  önbellekli snapshot'ı yeniden gönderir (paket kaybeden istemci kalıcı
  bayat kalmaz). `RoomConfig.max_snapshot_bytes` aşımı uyarı loglanır
  (rUDP MTU hazırlığı). **Ölçüldü (release, 1 hareketli + N hareketsiz
  oyuncu, adım p50):** 100: 270→15 µs · 200: 1125→34 µs · 400:
  5359→73 µs · 800: 66151→140 µs (~472×; 800'de 33 ms bütçesinin 2×
  üstündeydi, artık %0.4'ü). Regresyonlar: late-join (ilk yayında tüm
  dünya), stale-leave, kare hızından bağımsızlık — hepsi snapshot
  semantiğine uyarlandı ve yeşil; 2 yeni core testi (grup yalıtımı +
  private, sessizlik + keepalive). AOI/hücre bölme kapsam dışı.

- [x] **Broadcast tick mimarisi** — oda-başına pacer kaldırıldı; tek global
  ticker görevi `tokio::sync::broadcast` ile `TickInfo{tick, at}` yayınlıyor.
  Oda actor'ünün tek await'i `tick_rx.recv()`; tick gövdesi senkron, 5 fazlı
  (CONTROL → READ → CONVERT → SYSTEMS → BROADCAST). Oda hizi global hızı tam
  bölmeli (`run_every`); bölünmeyen hız `CoreError::TickRate` ile reddedilir.
  `Lagged` → uyarı + duvar saati `dt` ile catch-up; `Closed` → temiz çıkış.
- [x] **Per-user action channel** — her bağlantının kendi `Action` kanalı;
  conn actor `try_send` ile iletir (doluyken kendi girdisini atar, izolasyon).
  Kontrol düzlemi (Join/Leave/Shutdown) ayrı control kanalı; join/leave tick
  sınırında işlenir (≤1 tick, deterministik).
- [x] **Kare hızından bağımsızlık + catch-up cap** — `dt` duvar saati
  tabanlı (kaçırılan tick'ler bir sonraki adımda telafi); üst sınır
  `max_catchup = 4` periyot. Test: `gsb-game/tests/frame_independence.rs`
  (60 Hz vs 15 Hz oda, aynı 5.0 s sim süresi → aynı mesafe).
- [x] Küçükler: `ServerHandle` ticker `JoinHandle` + `stop()` kaskadı
  (registry → ticker.abort → accept.abort), config anahtarları
  (`room_control`, `conn_action`; `room_mailbox` kaldırıldı).

- [x] **#1 Join'de tam dünya snapshot'ı** — `last_sent` artık bağlantı
  başına; sonradan giren oyuncu ilk tick'te tüm dünyayı görür.
  Regresyon testleri: `gsb-game/tests/late_join.rs`.
- [x] **#2 `gsb-lint` yeniden çalıştırma + kapsam** — `check()` tarama
  kapsamındaki her dosya için `cargo:rerun-if-changed` yayınlar; kapsam
  `src/`+`tests/`+`examples/`; desenler `select!`/çıplak `Mutex`/`RwLock`
  ile genişletildi; `gsb-ecs` ve `gsb-protocol` lint'e bağlandı (6/7 crate).
  Poison testiyle doğrulandı: src-only değişiklik artık build'i patlatıyor.
- [x] **#3 Registry non-blocking** — join/leave odasıyla olan gidip-geliş
  bağlantı başına **dispatcher görevine** devredildi; registry hiçbir
  zaman oda await etmiyor.
- [x] **#4 Bildirim yolu kaybı + leave/rejoin sırası** — conns kaydı
  bağlantının bütün ömründe yaşar (inbox asla bırakılmaz); `PlayerLeft`
  entity taşır; stale leave (entity uyuşmazlığı) yok sayılır; join,
  bağlantının eski odasını temizleyerek girer. Test:
  `gsb-core/tests/registry.rs`.
- [x] **#5 `RoomId(0)` sentinel** — `ConnInfo.room: Option<RoomId>`.
- [x] **Tick coalescing** (o zaman pacer tabanlı; broadcast geçişinde yerini
  duvar saati `dt` catch-up'ı aldı — "biriken Tick" kavramı yok oldu).
- [x] Küçükler: reader pump double-`Closed`, `RoomConfig::conn_out_capacity`
  ölü kod, accept loop backoff, DESIGN.md iddia düzeltmeleri
  (fan-out O(dirty×conn) dürüst hali, "tek await" formülasyonu, lint kapsamı),
  registry shutdown'da break (kendi mailbox klonu EOF'u engelliyordu).

## P0 — Ölçüm (önce veri, sonra optimize)

- [ ] **Load test harness'i** — en kritik kalan iş; 100k iddiasının tek
  kanıtı bu olacak. Scripted client: N bağlantı aç → join → sürekli
  MOVE_TO; sunucu tarafında tick süresi (p50/p99), `dropped_frames/s`,
  bağlantı/oda sayıları. Mevcut e2e altyapısı üzerine (`tests/load.rs`
  veya `examples/loadgen.rs`).
- [ ] **Temel metrik** — periyodik (1 Hz) rapor: bağlantı sayısı, oda
  başına entity, tick ms, drop hızı. Basit sayaç + tracing; metrik
  kütüphanesi yok.
- [ ] **`MovementSystem` unit testleri** — room-seviye testler dolaylı
  kapsıyor; spawn → target → run → konum/arrive doğrulaması hâlâ yok.

## P1 — Robustluk ve güvenlik

- [ ] **Oturum zaman aşımı** — ölü TCP bağlantısı (RST'siz kopma) slot +
  görev + kayıt işgal etmeye devam ediyor. Heartbeat son-görülme damgası
  + aralıklı süpürme (kanal mesajıyla, kilit yok).
- [ ] **`RoomConfig.max_players` + doluluk yanıtı** — `CoreError::RoomFull`
  yeniden eklenecek; doluysa JOIN'de `ERROR (room full)`.
- [ ] **Güvenlik yüzeyi** — `Authenticator` trait'i (AUTH bugün no-op),
  bağlantı sayısı limiti, aksiyon rate-limit.
- [ ] **Koordinat formatı kararı** — `sfixed32` (tam sayı) wire vs `f32`
  simülasyon: 30 Hz × 10 u/sn'de tick başına 0.33 birim → istemci 3
  tick'te bir değişim görür, `bump()` her tick aynı tam sayıyı yayınlar
  (bant israfı + merdivenlenme). Float ya da mm cinsinden int kararı
  Unity tarafıyla birlikte (proto değişimi).
- [x] **Tick/Action kanal ayrımı** — broadcast tick + per-user action +
  control kanalı olarak çözüldü (bkz. Kapatılanlar). Kalan parça: bağlantı
  başına girdi rate-limit (güvenlik maddesi).
- [ ] **Entity bazlı yayın rate limit** — 30Hz snapshot yerine 10–15Hz +
  istemci interpolasyonu (Unity tarafında küçük ama gerçek iş).
- [ ] **Bağlantı başına Vec churn** — her tick × bağlantı yeni Vec
  allokasyonu; yeniden kullanılabilir buffer. (Load test verisiyle
  doğrulanacak — belki sorun değildir.)

## P2 — Ölçek (load test sonrasına göre sıralanır)

- [ ] **AOI / oda içi görünürlük** (`DESIGN.md` §8) — tek odada 100k
  bağlantı: 1.5 milyar frame teslimi/sn **CPU** duvarı. Oda segmentasyonu
  + `Visibility` trait'i.
- [ ] **Delta yayın** — son snapshot farkı; bant kazancı.
- [ ] **Registry şeritleme** — dispatcher tasarımı registry'yi bloke
  etmediği için bu artık yalnızca tablo bant genişliği sorunu; load test
  gösterirse room bazlı parçalar.
- [ ] **`QueryState` yeniden kullanımı** — oda başına bir kez kur, tick'te
  sadece `iter`.
- [ ] **Kompresyon (zstd)** — batch'ler üzerine transport seçeneği.

## P3 — Doküman ve politika noktaları

- [ ] **Kapanma bildirimi** — `stop()`'ta odadaki oyunculara sessiz
  disconnect yerine ERROR/leave frame'i.
- [ ] **Oda bazlı config override** — factory aynı config'i kullanıyor;
  yüksek yoğunluklu odalar için farklılaşma.
- [ ] **`gsb-client` yardımcı crate'i** — `read_frame` mantığı e2e testi
  ile örnek istemcide birebir kopyalanmış; tek yerde yaşatmak.

## Bilinçli olarak yapılmayanlar (referans)

- Cross-server / cross-region, kalıcılık, yük dengeleyici — v1 kapsam dışı.
- bevy `Event`/observer sistemi hot path'te kullanılmıyor — bilerek
  (snapshot kararı wire içeriğiyle; ayrı bir versiyon bileşeni yok —
  eski `EntityVersion`/`bump()` denetim turunda kaldırıldı; gerekirse
  delta yayınında (P2) yeniden getirilir; `DESIGN.md` §7).
