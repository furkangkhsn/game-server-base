# Ne kaldı? — gsb Geliştirme Listesi

Durum: v1 mimari tamam; dış incelemede bulunan 5 kritik hata (#1–#5)
kapatıldı. 19/19 test yeşil. Aşağıdakiler **ölçülmemiş performans**,
**robustluk** ve **güvenlik** başlıklarındaki kalan işler.

## Kapatılanlar (bu tur)

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
- [x] **Tick coalescing** — biriken Tick'ler tek tick'te işlenir, diğer
  mesajlar sıra korunarak; yavaş tick çöp iş biriktirmez.
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
- [ ] **Tick/Action kanal ayrımı** — kötü niyetli istemci odanın mailbox'ını
  doldurduğunda pacer'ın `send().await`'i sıraya girer (tick jitter).
  Kontrol düzlemi (Tick) ayrı kanal + ince merge görevi.
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
  (`EntityVersion` ile deterministik dirty tracking; `DESIGN.md` §7).
