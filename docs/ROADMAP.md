# Ne kaldı? — gsb Geliştirme Listesi

> **Kod doğrulama turu:** P1 maddelerinin kodda yeniden taranmasıyla üç
> maddenin durumu güncellendi — `max_players`/`RoomFull` fiilen kapanmış
> (join yolu + ERROR 8 + shard guardrail + test), `Güvenlik yüzeyi`'nin
> iki alt parçası (`TicketAuth` hook'u, `max_connections` cap'i) kapanmış
> (yalnız post-auth aksiyon rate-limit'i açık), `Oturum zaman aşımı`
> yarı kurulmuş (reader idle-timeout var; yarı-ölü bağlantı süpürmesi
> yok). Aşağıdaki maddeler bu doğrulamayı yansıtıyor.

Durum: v1 mimari tamam; dış incelemede bulunan 5 kritik hata (#1–#5)
kapatıldı; tick mimarisi broadcast tabanlı yeniden kuruldu (ayrı
`docs/TICK-ARCHITECTURE.md`); yayın fazı **grup başına tam dünya
snapshot'ı** modeline geçirildi (aşağıda); grup adilliği sözleşmesi
(grup başına defter) + F1/F3/F4 kapatıldı (aşağıda); `3f25874`'ün
kendi denetim raporundaki 5 bulgu (Bulgu 1–5) kapatıldı (aşağıda);
2aad7ea üstü denetim turunda F5 kapatıldı, F6 (wire byte iddiası)
ölçülerek düzeltildi, F4'ün "yapısal olarak tespit edilemez" hükmü
yeniden incelemeyle doğrulandı (aşağıda); wire kimliği kompaktlaştırıldı:
`EntityRecord.entity` tam bevy bits yerine oda-yerel monoton serial —
tipik kayıt 10 → 6 B (snapshot çerçevesi dahil 12 → 8 B), 1400 B eşiği
~117 → 171 entity'ye kaydı, kimlik değişmezi korundu (aşağıda);
yayınlanabilirlik ön koşulu (Position ⇒ broadcast) **yapısal** hale
getirildi — `on_join` dışından spawn edilen entity artık sessizce
görünemez, `Owner` ölü componenti kaldırıldı, aynı sınıftan kalanlar
tarandı (aşağıda); `WireId`'ün mint tarafı **tip düzeyinde
kapandı** (field private + `Default` kaldırıldı; tek inşaat yolu
crate-içi constructor, tek minting noktası odanın sayacı); **metrik
altyapısı** kuruldu — sayaçlar aktörlerin yerel durumunda, kanalla
toplayıcıya (oda tick'ine await eklemeden); **ilk uçtan uca yük testi**
alındı: 100/500/1000 gerçek TCP istemci, 30 Hz üç ölçekte de korundu,
drop 0, N ile büyüyen tek metrik adım süresi (aşağıda); **üç metrik
ölçüm hatası düzeltildi** (adım histogramı tick bütçesine göre → bütçe
aşımı okunur; örnek gönderim temposu rapor temposuna bağlandı + oran
örnek aralığı üzerinden; metrik kanalı bounded + `try_send`) ve **AOI**
kuruldu — tek odada bant genişliği O(entity) → O(görünürlük kümesi);
ana sorunun cevabı: `gsb-game` içinde, `gsb-core`'e **dokunmadan**
(500/1000/2000 + hücre boyutu taramasıyla ölçüldü, aşağıda); **görünürlük
artık sunucu seçimi** — aynı oyun üzerinde `all` / `spatial` (AOI) /
`team` (takım sisli) / `pvs` (sektör PVS) stratejileri, `Config.visibility`
ile seçilir, `Visibility` trait'i gerekmedi (seam = `RoomLogic`), ortak
oda muhasebesi `gsb_game::common`'de tek kopya; D1: p50 adım bütçesi
10 000'e kadar korunuyor (kuyruk 6k+ aşıyor, 10k'da in-proc istemci doyuğu),
D2: c5 %82 / PVS %49 bant tasarrufu, team bu yük geometrisinde sıfır
kazanc–2× kodlama, D3: overlap 1,0–8,28 ölçülüp "birim başına tek
kodlama" tasarımı eşik altında bırakıldı (aşağıda, "Kapatılanlar
(görünürlük stratejileri turu)"); **protokol-ihlal bütçesi** kuruldu —
bağlantı başına ağırlıklı ömür-boyu bütçe (16 puan), ilk 3 ihlal
cevaplanır sonra huni susar (cevap amplifikasyonu sınırlı), tükenince
ERROR 9 + kapatma, peer adresi sinyalle taşınır; ve **rUDP taşıması**
eklendi — tek soket, tek demux, cookie el sıkışması, kontrol/oyun band
ayrımı, MTU drop+count, BTreeSet idle sweep; e2e akışlarının 7/7'si
**her iki taşımada** aynı niyetle çalışıyor, loadgen `--transport
udp` ile TCP'ye karşı yan yana ölçüldü (150 istemci: 30 Hz korundu,
snapshot bant genişliği eşdeğer, handshake TCP'nin altındaydı);
**rUDP cookie key** artık OS entropisinden (`getrandom`) ya da
konfigürasyondan (`cookie_key`) — sessiz zayıf geri düşüş yok,
entropi yoksa süreç başlatmayı reddeder (aşağıda, A turu §cookie);
ve **oda segmentasyonu (`sharded`)** kuruldu — tek oda N shard
actor'üne bölünür, entity'ler sınırda migrasyonla taşınır (tek
sahip değişmezi, 1-tick hizalama, range-partitioned wire kimliği,
iki-tablo epoch şeması); ölçümle: 10k bağlantıda tek odanın
aştığı adım-duvarı `sharded N=4/8` aşılmaz, bedel ~1,4× sunucu CPU
(aşağıda, "Kapatılanlar (oda segmentasyonu turu)"); **input sıralama +
onay** kuruldu — istemci girdileri oturum başına numaralandırıldı
(yüksek-su kuralı), sunucu işlediği son sırayı bağlantı başına private
frame'le onayladı (işleme işareti — teslim garantisi taşıma
katmanında kaldı); ve **`spatial` stratejisi delta kodlamaya**
geçti — kodlama birimi grup değil hücre (hücre başına tick başına bir
kodlama, grup = kitle), full/delta (grup, hücre) başına, keepalive'da
taze full (yakınsama garantisi), geç girişte tek seferlik private full;
istemci gap kuralı wire kuantizasyonu bulgusuyla birlikte belirlendi
(akış olay-odaktır: gap normaldir, delta üzerine uygulanır,
baseline'sız atılır) — `still` yük profiliyle ölçüm: kayıt/tick 67-77×
az (hareketsizlik oranıyla artan kazanç), bant/conn 6-7× az, adım p50
~2× (hücre fark taraması), bütçe aşımı %0 (aşağıda, "Kapatılanlar
(delta yayın + input sıralama turu)").
Test sayısı: bugün itibarıyla **294** (294/294 yeşil; tarihsel
ilerleme 58 → ... → 279 için `docs/CHANGELOG.md` başlığına bakınız).
`#[ignore]`'lu gsb-lint doctest; hiçbir eski test silinmedi/ihmal edilmedi). Ara turlar: **reconnect/detach** (tasarım
`docs/RECONNECT.md`; core mekaniği + demo park/bot + global epoch düzeltmesi — aşağıda P1), **trait birleşimi + PlayerId**
(`docs/TRAIT-ARCHITECTURE.md` Faz 1-2; shard keepalive terfisi, RebindKey küçültmesi), **Faz 3** (shard-RPC + match-result,
`tests/rpc_shard.rs`) ve **ops yüzeyi** (`docs/OPS.md`: Prometheus `/metrics`, `/healthz`, admin API — elle yazılmış HTTP,
`MetricSink::Watch`, sıfır yeni bağımlılık; aşağıda "Kapatılanlar (ops yüzeyi turu)"). Son olarak **güvenlik turu**: rustls ile TLS taşıması (`docs/SECURITY.md` Tur A; tüm guardrail e2e'leri artık tcp+udp+TLS üçlüsünde), auth rate-limit, pre-auth frame bütçesi ve unauthed oturum cap'i (Tur B; aşağıda "Kapatılanlar (güvenlik turu)"). Son ekleme:
**dış inceleme hızlı düzeltme turu** — doğrulanmış dış inceleme raporundan beş
madde kapatıldı: doküman çürüğü (metrik kanalı "unbounded" iddiası — kod
bounded + `try_send` iken üç dosyada kendisiyle çeliyordu), `Ticker::spawn`
panik yolu (`hz ≤ 0`/NaN artık tipli hata), SIGTERM graceful shutdown,
README'nin kodla senkronu ve **READ fazı açlığı** (döner imleç — hash-sırası
öneki bütçeyi her tick tüketirken kuyruk bağlantıları sonsuza kadar
ulaşılmaz kalıyordu; mutation-verified testle kilitlendi). Aşağıda,
"Kapatılanlar (dış inceleme hızlı düzeltme turu)"; onu izleyen turda
**aktör supervision'ı** kuruldu — panik eden bir oda/shard task'i artık
registry tablosunda zombi `Running` kaydı bırakmıyor: ölüm izleniyor,
üyeler `RoomGone` ile haberdar ediliyor, opsiyonel `restart_on_panic`
politikası odayı fabrikadan boş olarak yeniden kuruyor (aşağıda,
"Kapatılanlar (supervision turu)"). rUDP REL bandının sessiz give-up'ı ise
**bilinçli olarak ertelendi**: rUDP deneysel statüsüne alındı, üretimde aynı
`Transport` seam'i arkasından kanıtlanmış taşıma koşacak (`udp.rs` modül
dokümanındaki "Status: experimental" beyanıyla). En son **tablo budama
turunda** shard'ların bağlantı-churn'üyle sonsuz büyüyen iki tablosu
budandı, metrik biriktiricileri kapanan varlıkları bırakacak şekilde
düzenlendi ve supervision turunun ortaya çıkardığı roster-drift panigi
düzeltildi (aşağıda, "Kapatılanlar (tablo budama turu)").

En son **dış inceleme düzeltme turu (park sızıntısı + eksen çelişkisi)**:
iki doğrulanmış bulgu kapatıldı.

1. **Park expiry registry satırını sızdırıyordu.** Grace room/shard
   tarafında biter (politika logic'in), ama oda bunu KİMSEYE söylemiyordu:
   registry'nin `detached` satırı, temsil ettiği entity despawn edildikten
   sonra da ayakta kalıyordu. Satırı serbest bırakabilen tek iki olay
   (aynı identity ile resume, odanın ölmesi) geri dönmeyen bir oyuncuda
   hiç gerçekleşmez — kalıcı odada terk edilen her oturum bir
   `max_connections` slotunu, sharded'da ayrıca bir `ShardGroup` üye
   slotunu KALICI tutuyordu (ölçüldü: 2 shard'lık odada park dolduktan
   sonra `members` 1'de çakılı kalıyor). Kod bunu "accepted v1 imprecision
   — caps close slightly early" diye belgelemişti; oysa RECONNECT §4 zaten
   "`members` sayacı … expire'te düşer" diyor: kabul edilmiş bir takas
   değil, uygulanmamış bir tasarım şartıydı. Düzeltme:
   `RegistryMsg::ParkExpired`; oda/shard `Despawn` kolunda raporluyor
   (senkron `try_send` — tick gövdesi await'siz kalır; DOLU mailbox
   sonraki tick'e kuyruklanır, çünkü düşen rapor sızıntıyı geri açardı;
   KAPALI mailbox'ta bırakılır). AI-handover kolu bilinçli olarak
   raporlanmaz: orada entity botla YAŞIYOR, slotu gerçekten tutuyor ve
   hâlâ geçerli bir resume hedefi (§9).
2. **`communication = "always-full"` + `visibility = "spatial"` sessizce
   yanlış yapılandırıyordu.** Çözümleyici kabul ediyor, `AlwaysFull`
   raporluyor, ama seçtiği oda (`Aoi` / `ShardedSpatial`) delta
   yayınlıyordu — üstelik testi bu ayrışmayı "honored as written" diye
   kilitlemişti. Ters yön (`delta` + all/team/pvs) zaten reddediliyordu;
   simetrik hale getirildi (`SingleAlwaysFull` / `ShardedAlwaysFull`).
   Sonuç: KABUL EDİLEN hiçbir `ResolvedSelection`, odasının konuşmadığı
   bir communication raporlayamaz. Anahtar YAZILMAMIŞSA türetme
   değişmedi, yani eksene hiç dokunmamış config etkilenmez.
   `config.example.toml`'un desteklenen-kombo tablosu da Faz B'den beri
   çürüktü (sharded × spatial'ı hâlâ "reddedilir" diyordu), yenilendi.

Test 292 → **294** (ikisi de mutation-verified; süit tamamen yeşil,
clippy 0 uyarı).

En son **okunabilirlik turu**: kaynak ağacı modül dizinlerine bölündü —
**40 dosya → 207**, en büyük dosya **5541 → 663**, medyan ~600 → ~190.
Saf yeniden düzenleme; davranış değişmedi (294 test yeşil, clippy 0
uyarı, loadgen 30 Hz / 0 hata).

Turun asıl işi dosya taşımak değil, **fonksiyon çıkarmaktı** — ve üç dev
fonksiyonun üçü de kesme noktalarını zaten kendi yorumlarında yazmıştı:

- `registry::run` 845 → **206**: 16 mesajlık dispatch'ten 8 kol `on_*`
  metoduna çıktı. Kollardaki 7 `continue` `return` oldu — match döngünün
  tek ifadesi olduğu için eşdeğer, ve derleyici `continue`'un fonksiyon
  sınırını geçmesine izin vermediği için dönüşüm mekanik olarak güvenli.
- `room::step_phases` 553 → **119** ve `shard::step_phases` 811 → **204**:
  `// -- Phase 0b`, `// -- Phase 1 — READ` banner'ları kesme noktalarıydı;
  fazlar arası geçen tek yerel değişkenler `ctx`, `actions`, `requests`.
  Shard'da `phase_migrate`/`phase_border`'a tick bilgisi artık imzada
  açıkça geçiyor.
- `conn::handle_frame` 381 → **88**: AUTH (182) ve JOIN (113) kolları
  metoda çıktı.

**Yöntem kuralı (yeni turlar için bağlayıcı):** bir struct'ın impl'i
bölünürken KARDEŞ değil ÇOCUK modül kullanılır. Çocuk atasının private
alanlarını gördüğü için `RoomActor`/`ShardActor`/`ConnectionActor`/`Demux`
alanları kendi ağaçlarında private kaldı — hiçbiri `pub`/`pub(crate)`
olmadı. Testlerin gördüğü alanlarda `pub(in crate::room)` var; bu
genişletme DEĞİL, bölünmeden önceki görünürlüğün aynısı (testler zaten
aynı modüldeydi).

**Hedefi aşan 44 dosya bilinçli** ve üç sınıfa ayrılıyor: (1) Rust'ın
izin vermediği yerler — trait ve trait impl tek blok olmak zorunda
(`GameLogic`, her odanın `impl GameLogic`'i); (2) tek sürekli prosedür
(`broadcast_phase`, `print_report`, `orchestrate`, `start_inner`) — ikiye
bölmek yarı kurulmuş durumu modül sınırından geçirmek olurdu; (3) tek
`match` — `shard/actor/messages.rs` (463) kollarını çıkarmak DENENDİ ve
GERİ ALINDI: kollar epoch/binding guard'larını paylaşıyor, ayırınca
sıralama argümanı dosyalara dağılıyor. `registry::run`'da aynı iş
çalıştı çünkü orada kollar bağımsızdı — fark kodun kendisinde.

En son **yayın paketi turu** (HANDOFF iş sırası 1): MIT `LICENSE`,
`rust-version = "1.95.0"` + `rust-toolchain.toml` 1.95.0'a sabit (alt
sınır `bevy_ecs 0.19.1`'den; 1.94.1 reddediliyor), `repository` yer
tutucusu kaldırıldı, `.github/workflows/ci.yml` (fmt check · clippy
`-D warnings` · test), `CONTRIBUTING.md`. `cargo fmt --check` artık
temiz (201 dosyalık saf format kommiti; fmt'nin import sıralamasının
açığa çıkardığı tek yedek glob import önden ayrı kommitte kaldırıldı).
Test 294 → 294. İki bulgu P3'e yazıldı. Ayrıntı: CHANGELOG "yayın
paketi turu".

Aşağıdakiler **ölçülmemiş performans** (10k+ ölçek aşağıda ölçüldü;
kalanı çok makine dağıtımı, congestion control), **robustluk** ve
**güvenlik** başlıklarındaki kalan işler.


Tamamlanan tüm turların ayrıntılı kaydı: **`docs/CHANGELOG.md`**.
- [ ] **Cross-seam etkileşim paketi** — in-process sharding'i "neredeyse
  invasif olmayan" seviyeye taşıyan üç parça (dış danışma diyaloğundan;
  tasarım notu CROSS-SHARD §2–§4 + bu madde):

  1. **Ödünç kayıtların gameplay'e açılması** — borrowed view zaten
     snapshot'a akıyor; aynı veriye hedefleme/sorgu amaçlı ergonomik
     erişim (`local ∪ borrowed` birleştirici yardımcısı). Sistemler
     sınır dibindeki yabancıları GÖRÜR hale gelir (~1 tick bayatlık
     kabulüyle).
  2. **`RemoteEffect` primitifi** — yabancı hedefe etki = otoriteye
     idempotent mesaj (`ShardMsg::RemoteEffect { target_identity,
     epoch, seq, payload }`). Vuruş/hasar/büyü hepsi buna biner;
     anti-cheat atıcı shard'ında.
  3. **Crystallization tetikleyicisi (histeresizli)** — hedef komşu
     shard'da ve K tick'tir etkileşim sürüyor → proaktif migrate;
     dövüş tek shard'a kristalleşir. Ping-pong'u önleme bandı dahil.

  Kapsam notları: AoE sorgularında `local ∪ borrowed` birleşimi logic
  tarafında manuel yapılır (bayatlık semantiği korunur); gerçek çoklu-
  link karma mod testi ≥3-shard rig gerektirir.

> **Devam notu (oturumlar-arası):** (1) Çoklu-listener'a QUIC/WS kapıları
> **KAPANDI** (çoklu-listener maddesinin son paragrafı; tur kaydı:
> CHANGELOG "çoklu-listener'a QUIC + WS kapıları turu"). Kalan tek
> sözleşmeli tur: (2) `team × sharded` export — üç ayrı ajanda erken
> sonlandı; sözleşmesi: `docs/CROSS-SHARD.md §8`; taze bağlamla TEK TUR
> halinde yapılmalı. Ortam notu: yeni bağımlılık indirilecekse cargo
> komutları `CARGO_HOME=$PWD/.cargo` ile koşulmalı (HOME önbelleği
> salt-okunur olabilir). Kommit disiplini: ajan aktifken asla `git add -A`.

## P0 — Ölçüm (önce veri, sonra optimize)

- [x] **Load test harness'i** — kapatıldı: `gsb-loadgen` binary'si +
  duman testi; 100/500/1000 ham sayılar, ilk doyma analizi ve burst
  bağlantı bulgusu "Kapatılanlar (metrik + yük turu)" bölümünde. 100k
  ölçeği hâlâ ölçülmedi (P2 — AOI/oda bölme sonrasına göre planlanır).
- [x] **Temel metrik** — kapatıldı: `gsb_core::metrics` +
  `MetricsCollector` (1 s rapor; log/kanal sink); kapsam ve tasarım
  "Kapatılanlar (metrik + yük turu)" + DESIGN §12'de.
- [ ] **Kalan metrik sayaçları için doğru-yol testleri** — reject
  bucket'larının (bu turda kapatıldı) aynı sınıfındaki kalan boşluk:
  "doğru yolda arttığını" doğrulayan testi OLMAYAN sayaçların tam listesi
  "Kapatılanlar (reject-bucket wiring + sayaç envanteri turu)"
  bölümündeki tabloda. Kısaca: RoomSample `step_min/sum_us`,
  `step_fine_hist` (oda tarafı), `late_*`, `lagged_*`,
  `keepalive_resends`, `snap_bytes*`, `snap_overflows`, `snap_records`,
  `shipped_*`, `private_frames`, `leaves`,
  `requests_local/external/timed_out/late`, `pending_requests`;
  RegistrySample `rooms_created/destroyed`, `joins/leaves`,
  `opens/closes`; ConnSample `frames_in/out`, `violations`, `last`;
  UdpClientStats `retrans_out`, `dup_in`, `oob_dropped`, `gave_up`.
  Yöntem: sayaç başına yolu tetikleyip artışı assert eden test (altyapı
  deseni: `tests/rpc.rs`'te `latest_room_sample` + adım başına
  `metrics_cadence_hz`). Öncelik önerisi: operasyonel sinyaller
  (cap/overflow ailesi + `requests_*` kardeşleri), sonra süre/
  histogram ailesi, en son log-düzeyi değerler.
- [ ] **`MovementSystem` unit testleri** — room-seviye testler dolaylı
  kapsıyor; spawn → target → run → konum/arrive doğrulaması hâlâ yok.

## P1 — Robustluk ve güvenlik

- [x] **Reconnect/reattach** — kopan oyuncunun entity'sini oyun
  politikasıyla (Park / AI devri / combat-held) yaşatan detach-resume
  mekanizması; oda sınıfı (persistent/ephemeral) ve ERROR 12 dahil.
  Tasarım dış danışma ile sabitlendi (`docs/RECONNECT.md`) ve iki alt
  turda uygulandı: Tur A (core mekaniği: Detach/Resume mesajları,
  epoch-guard'lı broadcast-resume, tek noktadan RebindKey, kanal swap,
  deadline sweep, persistent/emeklilik) + Tur B (demo park politikası,
  bot stub gerçek ingest yolunda, e2e same-wire-id kanıtı, churn profili:
  100 istemci × 3 döngü → 300 resume ~37/sn, sıfır hata, 30 Hz korundu).
  Tur B'nin bulgusu: join epoch'ları bağlantı başına mintleniyordu ve
  kimliğin sonraki HER resume'u bir kez haksız stale-reject yiyordu
  (ölçüm: 20 istemcide 20 red) — epoch minting registry'ye global
  taşındı, regresyon kilidi `repeated_reconnects_are_accepted_on_first_
  attempt`. Kalan not: oda içi anahtarların uzun vadede PlayerId'ye
  taşınması (RECONNECT §14.1); rUDP üstünde e2e varyantı (deneysel
  statüye bağlı).

- [~] **Oturum zaman aşımı** — *yarı kurulmuş.* Reader pump'un idle
  zaman aşımı (`idle_timeout_secs`, vars. 30 sn) **istemci sessiz**
  durumunu yakalıyor: hiçbir frame gelmezse pump `ConnIn::Closed`
  gönderir, teardown kaskadı (registry → oda `Leave`) çalışır. Kalan
  boşluk **yarı-ölü bağlantı**: istemci hâlâ heartbeat atıyor ama karşı
  taraf gitmiş (RST'siz kopma) — bu, idle pencereyi *tetiklemiyor*
  (trafik var), slot + görev + kayıt işgal etmeye devam ediyor. Çözüm:
  heartbeat son-görülme damgası + aralıklı süpürme (kanal mesajıyla,
  kilit yok) — mevcut detach-hold sweep'ine (`room.rs` Phase 0c) bir
  besleyici. 10k cap'li odada zombi slot'ların temizlenmesi, cap'ın
  *gerçek* kapasiteyi yansıtması için şart.
- [x] **`RoomConfig.max_players` + doluluk yanıtı** — kapatıldı: join
  yolu cap'i kontrol ediyor (`room.rs` — `conns.len() >= cap` →
  `CoreError::RoomFull`, entity/kanal/durum oluşturulmaz), connection
  actor `RoomFull`'u ERROR code 8'e map'liyor (bağlantı canlı kalır,
  başka odaya join olabilir), registry shard yolunda da aynı guardrail
  (`registry.rs`), regresyon kilidi `room.rs` testinde (`max_players:
  Some(2)`). ROADMAP'in eski "bugün kurulmuyor" notu yayınlabilirlik
  turu öncesine ait — tarama tamamlandı ve kuruldu.
- [~] **Güvenlik yüzeyi** — *iki alt parça kapatıldı, biri açık.*
  `Authenticator` → **`TicketAuth`/`TicketValidator` hook'u kuruldu**
  (`gsb_core::auth`: platform'a delegasyon — sunucu platform'un keşfettiği
  ticket'ı doğrular, kendi kimlik sistemi kurmaz; `Fn(Bytes) -> Future`
  şekli, timeout, amplification bound, config'ten `ticket: Option<TicketAuth>`
  ile besleniyor; `None` = legacy local-auth yolu, tüm eski akış
  değişmez). Bağlantı sayısı limiti → **`max_connections` cap'i
  kuruldu** (registry `ConnOpened`'te enforce + pre-auth cap türetimi
  `max(cap/4, 64)`, SECURITY §4). Kalan: **post-auth aksiyon
  rate-limit** (bugün yalnızca pre-auth rate-limit'leri var — AUTH
  penceresi, pre-auth heartbeat, pre-auth frame bütçesi; post-auth girdi
  rate-limit'i yok, Tick/Action maddesinde de notlu).
- [ ] **Koordinat formatı kararı** — `sint32` (zig-zag varint, tam sayı) wire vs `f32`
  simülasyon: 30 Hz × 10 u/sn'de tick başına 0.33 birim → istemci 3
  tick'te bir değişim görür (delta turunda bu kuantizasyon **gap
  kuralına dâhil edildi**: wire konumu değişmeyen tick'ler grup
  stream'inde normal boşluktur — istemci delta'yı üzerine uygular,
  yakınsama garantisi keepalive full'ıdır; "Kapatılanlar (delta yayın
  + input sıralama turu)" C maddesi). `bump()` her tick aynı tam sayıyı
  yayınlar (bant israfı + merdivenlenme). Float ya da mm cinsinden int
  kararı Unity tarafıyla birlikte (proto değişimi).
- [x] **Tick/Action kanal ayrımı** — broadcast tick + per-user action +
  control kanalı olarak çözüldü (bkz. Kapatılanlar). Kalan parça: bağlantı
  başına girdi rate-limit (güvenlik maddesi).
- [ ] **Entity bazlı yayın rate limit** — 30Hz snapshot yerine 10–15Hz +
  istemci interpolasyonu (Unity tarafında küçük ama gerçek iş).
- [ ] **Bağlantı başına Vec churn** — her tick × bağlantı yeni Vec
  allokasyonu; yeniden kullanılabilir buffer. (Load test verisiyle
  doğrulanacak — belki sorun değildir.)

- [ ] **Konfigürasyon düzeltmesi: üç eksenli seçim — topology × visibility ×
  communication** (dış danışma bulgusu): mevcut beş strateji
  (all/spatial/team/pvs/sharded) iki FARKLI kavramı tek seviyede
  birleştiriyordu. Doğru model üç dik eksendir:

  | Eksen | Sorar | Değerler |
  |---|---|---|
  | `topology` | Dünyayı kim/sahip olarak nasıl hesaplıyor? | `single` \| `sharded(N)` |
  | `visibility` | Bir birimin İÇİNDE kim kime hangi veriyi gönderecek? | `all` \| `spatial` \| `team` \| `pvs` |
  | `communication` | Veri nasıl paketlenip taşınacak? | `always-full` \| `delta` (N delta + 1 full yakınsaması) |

  Her kombinasyonun anlamı olmayabilir — kullanım anında belli olur;
  desteklenmeyenler başlangıçta reddedilir/belgelenir.

  - **Faz A:** ✅ Kapatıldı — üç eksenli config yüzeyi (`Topology`/
    `Communication`/legacy `visibility` girdi-kodlaması),
    `resolve_selection()` tek geçiş kapısı, desteklenmeyen kombolarda
    faz-bilgili tipli hatalar; 14 test (`config_axes.rs`). Test 265 → 279.
  - **Faz B:** `sharded × spatial` kompoziti — her shard kendi içinde
    hücre-gruplu yayın yapar (AoiRoom mantığının shard-içi örneklanması);
    ödünç border şeridi delta defterine entegre edilir (borrowed set
    her tick tam geldiğinden diff'i önceki borrowed görünümüne karşı
    kurulmalı — yoksa her tick dirty olur).
  - **Faz C:** ✅ Kapatıldı — `ShardLink::exchange_mode()` seam'i:
    InProc ⇒ AlwaysFull (byte mpsc-move'da bedava, lokal CPU kıt;
    CROSS-SHARD §7 A/B ölçümü), Ipc/Net ⇒ Delta (gelecek; DISTRIBUTED
    §4b codec sahipliği). Karışık-mod mahalle desteği testli.
    Test 284 → 287.
  - Zemin hazır: `GameLogic` sözleşmesi mod-farkını destekliyor
    (snapshot bool + keepalive hook); **delta motoru Faz B'de ortak
    bileşen olarak common.rs'e çıkarıldı** (`CellBook`/`CellPieces` —
    aoi ve sharded paylaşıyor) → "ortak codec çıkarımı" maddesinin
    yarısı gerçekleşti; kalan: team/pvs/all stratejilerinin bu motora
    adoptasyonu (tetikleyicili — demo ölçeğinde kazanç yok).
    `BorderRecord<Strip>` jenerikliği ve `ShardLink` seam'i Faz C'nin
    primitifleri olarak duruyor.
  - [ ] **Kalıcılık** — iki ayrı sınıf olarak tasarlandı:
    `docs/PERSISTENCE.md` (maç oyunları: her-tick typed checkpoint +
    çift-tampon + çökme devre kesicisi; MMO: event-based + periodic
    checkpoint, otorite merkezi katmanda). Uygulama tetikleyicili.
- [ ] **`team × sharded` kompoziti** — tasarım HAZIR:
    `docs/CROSS-SHARD.md §8` (registry-hub byte-encoded takım-export;
    RegistryMsg monomorfik kalır, codec sahipliği logic'te — DISTRIBUTED
    §4b ilkesi; TTL sweep + fan-out + izolasyon kuralları dahil).
    Tetikleyici: gerçek takım-tabanlı oyun ihtiyacı. Uygulama taze
    oturumda sözleşmeyle yapılır.

## P2 — Ölçek (load test sonrasına göre sıralanır)

İlk yük turu (100/500/1000 — "Kapatılanlar (metrik + yük turu)")
sıralamayı destekledi: N ile büyüyen tek metrik **adım süresi**
(fan-out maliyeti) ve registry'ye tek bir baskı sinyali gelmedi
(drop 0, late ≈0, bağlantı/oda olayları sönük). Sıra: AOI önce,
delta sonra; şeritleme veri gelmedikçe dokunulmaz.

- [~] **AOI / oda içi görünürlük** (`DESIGN.md` §8) — tek odada 100k
  bağlantı: 1.5 milyar frame teslimi/sn **CPU** duvarı. **Tek-oda mekansal
  AOI önceki turda kapatıldı**; sonraki turda görünürlük **strategi
  seçimi** oldu (`all`/`spatial`/`team`/`pvs`, `Config.visibility`),
  break-even önce in-proc (p50 10k'ya kadar bütçe altında, kuyruk 6k+
  aşıyordu — istemci doyuğu karışıktı) ve bu turda **ayrı prosesle net
  ölçüldü**: p50 bütçeyi **9k-10k arasında** aşıyor (10k: ≥50 ms, %54,8
  adım bütçeli üstte, `server_hz` 23,2); 10k'da sunucu çekirdek havuzu
  **%25** dolu → duvar **tek room actor'ünün serisel adım yolu**.
  `Visibility` trait'i gerekmedi (C maddesi). **Kalan maddeler
  kapatıldı:** oda segmentasyonu → `sharded` (oda segmentasyonu turu,
  §8.2); "birim başına tek kodlama" → delta turunda hücre = kodlama
  birimi olarak gerçekleşti (overlap_x: still 0.90'da 5.77 → 0.09;
  "Kapatılanlar (delta yayın + input sıralama turu)").
- [x] **Delta yayın** — kapatıldı (delta turu): `spatial` stratejisi,
  son snapshot farkı değil **hücre başına içerik farkı** (kodlama birimi
  hücre, grup = kitle); `still` profiliyle ölçülen kazanç: kayıt/tick
  67-77×, bant/conn 6-7× (hareketsizlik oranıyla artan), adım p50 ~2×,
  bütçe aşımı %0. Detay + elenen alternatifler: "Kapatılanlar (delta
  yayın + input sıralama turu)" B/C maddeleri. Kalan (genelleme notu):
  birim = tam-görünürlük birimlerinin birleşimiyle ifade edilebilen
  **en kaba** parçalama — `spatial`'da hücre; takım sisli/oyuncu-bazlı
  aydınlatılmış hücre saklaması bu ilkenin bir sonraki adımı (P2,
  aşağı).
- [ ] **Registry şeritleme** — dispatcher tasarımı registry'yi bloke
  etmediği için bu artık yalnızca tablo bant genişliği sorunu; load test
  gösterirse room bazlı parçalar. (İlk turda göstermedi: 1000
  bağlantıda opens/closes/joins akışı adıma hiç gölge düşürmedi.)
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
- [ ] **`protoc-bin-vendored` bağlı değil** — `gsb-protocol` build-dep
  olarak bildiriyor, `build.rs` panik mesajı "using vendored protoc"
  diyor, ama build sistem `protoc`'unu çağırıyor
  (`PROTOC=/nonexistent/protoc` ile düşüyor; CI `protobuf-compiler`
  kuruyor). Ya bağla ya bağımlılığı ve mesajı kaldır (yayın paketi
  turu bulgusu).
- [ ] **`loadgen/churn.rs` log string'inde gömülü boşluk blokları** —
  "stale … resume … same … connection" metni iki yerde 38'er boşluk
  taşıyor (yayın paketi turu bulgusu; rustfmt literal'e dokunmaz).

## Bilinçli olarak yapılmayanlar (referans)

- Cross-server / cross-region, kalıcılık, yük dengeleyici — v1 kapsam dışı.
- bevy `Event`/observer sistemi hot path'te kullanılmıyor — bilerek
  (snapshot kararı wire içeriğiyle; ayrı bir versiyon bileşeni yok —
  eski `EntityVersion`/`bump()` denetim turunda kaldırıldı; gerekirse
  delta yayınında (P2) yeniden getirilir; `DESIGN.md` §7).
