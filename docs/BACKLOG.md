# BACKLOG — iş sırası ve bırakılanlar

Bu belge, bilerek **yapılmamış** her şeyin tek yeridir. Diğer belgeler
bir maddeyi ayrıntılı anlatabilir; ama "ne açık, neden bekliyor, ne
tetikler" sorusunun cevabı burada. Bir madde kapanınca buradan silinir
ve CHANGELOG'a geçer; yeni bir erteleme doğunca buraya eklenir.

İlke (kullanıcı kararı): önce bugün yanlış olan, sonra doğrulamayı
engelleyen, en son özellik. Her iş, sebebinin yaşadığı katmana gider
("her şeyi bir yere yığmadan").

## 1. İş sırası (2026-09-25'te onaylandı — TAMAMLANDI aynı gün)

Her paket kendi turu: ayrı worktree, alt ajan, ebeveyn doğrulaması +
bağımsız mutasyon, `main`'e fast-forward. Push yok. Bütün paketler
kapandı (testler 657 → 818); sıradaki iş §2'den seçilir.

| # | Paket | Yeri | Durum |
|---|---|---|---|
| 1 | **D — göç tick'i ölümlü kopyası:** göç eden entity kaynak dünyada bir tick daha duruyor, o tick'teki YEREL darbe kopyaya iniyor | `gsb_kit::sharded` (+ MMO testi) | ✅ `2a94e3a` (CROSS-SHARD §4d) |
| 2 | **K4 — oyuncu kimliği → ev shard'ı:** kayıtlı karakter oturuma bağlı, gerçek sunucuda herkes shard 0'da | çekirdek (ince seam: `home_shard` + join kancasına doğrulanmış kimlik) + kit + MMO. Anahtar: ticket'ın `player`'ı; ticket'sız eski yol (`Auth.name`) yalnız geliştirme yolu olarak belgelenir | ✅ `934bb14` (GAME-MODULE "K4 sonucu") |
| 3 | **U — rUDP parçalama:** kümelenmiş MMO `snap_overflows` + arena G3-1 (full'lar 1400 B'yi aşıyor); rUDP 1472 B üstü datagramı atıyor | `gsb-net/udp` (oyun bandı, sunucu→istemci). Kit/çekirdek zarfı değişmez | ✅ `1526fb0` (DESIGN §6 "MTU") |
| 4 | **Küçük paket:** G3-3 (arena istemcisi takımını wire'dan öğrenemiyor), G3-2 (join'de delta'lar private full'dan önce — inceleme), loadgen CLI hataları panikle, rustdoc uyarıları + CI doc kapısı, `max_detach_hold` sunucu config'inde yok, `RoomSample`'da zorlanmış bırakma / uzak etki / crystal sayaçları, göç sayısı raporu (F3) | her biri kendi crate'inde, ayrı commit | ✅ `183f32d` (CHANGELOG "Küçük paket turu") |
| 4a | **S — çekirdek kapanış kilitlenmesi** (U'da bulundu): `stop()` ticker'ı hemen iptal ediyor; registry dolu oda kontrol kanalında `send().await`'te bekliyor, oda bir daha tick atmıyor, broadcast kapanmıyor (registry bir `Ticker` tutuyor) — stop anında >128 canlı üye koparsa sunucu asılı | `gsb-core` (registry/ticker kapanış sırası) | ✅ `0c88af0` (DESIGN §9.1) |
| 4b | **H — rUDP el sıkışma proof kaybı** (U'da bulundu): istemci proof'u gönderince kendini bağlı sayıyor; 200+ eşzamanlı el sıkışmada loopback'te sunucu soketinin alım kuyruğu taşıyor, proof kayboluyor, istemci ölüyor | `gsb-net/udp` (kabul `ACK{1}` + proof yeniden gönderimi, sunucu idempotent) | ✅ `717aff2` (DESIGN §6 "El sıkışma kaybı") |
| 5 | **T — takım odasında delta:** arena yalnız full gönderiyor (bağlantı başı ~234 KB/s @1000) | `gsb_kit::team` (ortak delta motoru) | ✅ `b2282df` (KIT-ARCHITECTURE §10 "T sonucu"; arena kazancı ~%10 — bulgu) |
| 6 | **W1 — team × sharded kompoziti** (CROSS-SHARD §8b; K4'ten `spawn_team_player_as`) | kit + çekirdek (registry hub, TEAMS fazı) | ✅ `95dd007` (CROSS-SHARD §8b.7) |
| 6b | **W2 — doğrulama oyunu "Cephe":** üç fraksiyon, takım sisi, 2×2 shard; müttefik harita geneli, düşman herhangi bir müttefiğin görüşüyle | yeni demo crate + sunucu modülü (`game = "war"`) + loadgen botu + ölçüm | ✅ `c2c3dc4` (KIT-ARCHITECTURE §10 "W2 sonucu", CROSS-SHARD §8b.8) |

## 2. Bırakılanlar

Tetikleyici yazılmamışsa "—". Kaynaklar dosya:satır (2026-09-25).

### A. Tetikleyicili / performans

| Madde | Tetikleyici | Kaynak |
|---|---|---|
| A1 Ortak delta motorunun `all` ve `pvs` odalarına adopsiyonu (team T'de yapıldı: `common::SetLedger`; oda başına ~30 satır + sunucuda `communication = "delta"` kabulü) | çoğu birimi duran bir iş yükünde ölçülebilir kazanç | KIT-ARCHITECTURE §10 "T sonucu", ROADMAP:649 |
| A2 `IpcLink` / `NetLink` (`KitMig`, `BorderRecord`, `RemoteEffect` codec'leri dahil) | bir soket çekirdekleri doldurur → Ipc; iki makine kararı → Net | DISTRIBUTED:107-138, HANDOFF:192 |
| A3 Uyuyan delta border değişimi (yalnız testte koşuyor) | ilk Ipc/Net link | CROSS-SHARD:541-557, 655 |
| A4 NUMA ölçümü, olumluysa soket başına süreç | bir soketin çekirdekleri dolar | DISTRIBUTED:236-256 |
| ~~A10 Entity başına yayın hızı~~ **Yapıldı (2026-09-25, `cb0b7dd`):** kit'te opt-in `RecordCodec::send_every` (varsayılan her tick, bayt ve istemci kuralı aynı); arena 15 Hz ile −43…−45 %; hangi oyunun hangi hızla açacağı ve istemci interpolasyonu oyunun kararı | ✅ | KIT-ARCHITECTURE §10 "A10" |
| A11 Bağlantı başına tick'te yeni `Vec` | yük verisi sorun gösterirse | ROADMAP:617 |
| A13 Registry striping (W1'den beri takım röleleri de registry'nin tek görevinde veri düzlemi yükü) | `team_export_drops_full > 0` ya da registry gecikmesi ölçülürse (W2: 1000'de 120 export/sn, ~281 k kayıt/sn röle, 0 düşme) | ROADMAP:700 |
| A14 Batch üstü zstd | — | ROADMAP:706, DESIGN:890 |
| A15 100k ölçeği (çok makine, congestion dahil) | — | ROADMAP:325, 364 |
| A16 RPC işçi havuzu | ~10k üyeli oda ya da oda tavanı gerçekten bağlarsa | RPC-CONTROL-PLANE:469 |
| A17 Doluluktan türetilen RPC pending tavanı | reject kovaları tavanın bağladığını gösterirse | RPC-CONTROL-PLANE:274 |
| A18 Adaptif border genişliği | — (ölçüm öncesi optimizasyon) | CROSS-SHARD:669 |
| A19 Dinamik adaptif tick hızı | — | DESIGN:1213 |
| A20 Join/leave için tick içi hızlı yol | gerekirse | DESIGN:1221, TICK-ARCHITECTURE:134 |
| A21 Dinamik oda bölme/birleştirme, bölgeler arası | — | DESIGN:892 |
| A22 Değer düzeyinde delta — faz 0 yapıldı (KIT-ARCHITECTURE §10 "A22"): mutlak sıkıştırmanın üstüne katkısı −2…+8 puan (MMO +23), göreli şema rUDP'de bayatlığı 5–10× artırıyor; biçim olursa (b) `base_seq` + resync isteği + çekirdek düşme sinyali (F11 ✅) + ithal kayıtlar için `RecordCodec::decode` | motor önkoşulları tamam (A30 ✅, A31 ✅, A10 ✅); tetik: bunları açtıktan sonra hâlâ bant/MTU baskısı gören AOI tipi bir oyun | KIT-ARCHITECTURE §10 "A22" |
| A23 Demo sunucusunda `team × delta` (config şu an reddediyor; kit hazır) | demo'da takım delta'sı isteyen koşu | KIT-ARCHITECTURE §10 "T sonucu" |
| A25 Takım export'u her tick; `every k` temposu (W2 ölçtü, gerekmedi: 1000'de 0 düşme; tempo istemci baytını değiştirmez, görmeyi k tick geciktirir) | `team_export_drops_full > 0` ya da registry gecikmesi | CROSS-SHARD §8b.1 |
| A27 Harita geneli nötrler (ele geçirme noktası) — bugün kendi shard'ında herkese, başka yerde sisle; W2-1 kanıtı: bölüm artefaktı (shard 3 oyuncusu 640 m ötedeki sahipsiz noktayı görüyor, 100 m'deki shard 0 oyuncusu görmüyor); en küçük değişiklik varlık başına "harita geneli" bayrağı | herkesin durumunu görmesi gereken bir hedef | CROSS-SHARD §8b.5 |
| A28 Göçte ayrılan shard'daki müttefikte bir tick'lik görünürlük boşluğu (kabul; W2'de ölçülmedi — loadgen kısa kayıp-geri gelişleri saymıyor) | bir oyunda görünür titreme raporu | CROSS-SHARD §8b.5 |
| ~~A29 Takım bütçesi üyeleri wire sırasıyla kesiyor~~ **Yapıldı (2026-09-26, `06d54b2`):** opt-in `with_export_rank` — kademe içinde oyunun sırası (önce üyeler korunur, eşitlikte küçük wire id), doğrusal seçim; varsayılan bayt aynı; kesme `RoomSample::team_over_budget`'ta | ✅ | KIT-ARCHITECTURE §10 "A29" |
| ~~A30 Kompakt wire id~~ **Yapıldı (2026-09-25, `9dfab9e`):** iç içe basım `(n − 1)·N + i + 1`; shard'lı oyunlarda −7…−15 %, id varint 2,6–3,2 → 1,7–1,9 B; istemci kuralı ve zarf aynı | ✅ | KIT-ARCHITECTURE §10 "A30" |
| A32 Shard'da oyuncu id'si ile wire id'ye ayrı sayaç (join bugün iki çekim yapıyor; ayrılırsa shard'lı wire id'ler kabaca yarıya iner) — tükenme koruması iki sayacı birlikte saymalı | shard'lı bir oyun A10/A31'i açtıktan sonra bant yine sıkışırsa | KIT-ARCHITECTURE §10 "A30" |
| A33 rUDP yazıcısının `frag_messages`/`frag_datagrams` sayaçları loadgen RESULT'ına (bugün yalnız yazıcı oturum logunda) | parçalanma ölçümü gerektiğinde | KIT-ARCHITECTURE §10 "A31" (A31-2) |
| ~~A31 Paketli kayıt koşusu~~ **Yapıldı (2026-09-25, `9a1de23`):** `RecordCodec::RUN` + `ClientDecoder::run_record`, `kit.proto` `records = 6`; savaş opt-in −54 % (TCP 200/500), rUDP datagram/kare yarıya | ✅ | KIT-ARCHITECTURE §10 "A31" |
| A24 İstemci tarafında delta uygulaması maliyeti (orkestre arena 1000'de `clients_cpu_s` +%15) — Not (B50): +%15 tek worker'lı koşullarda ölçüldü; B50 A/B'yi yeniden almadı (bugün arena 1000 `clients_cpu_s` 13,5–14,5) | yük verisi sorun gösterirse | KIT-ARCHITECTURE §10 "T sonucu" |
| A34 Seam ötesi görüş şerit genişliğiyle sınırlı: bir kaynağın yarıçapı (oda ya da `SightRadius`) şeritten büyükse komşu shard'daki şeridin ötesini görmez (orada bilinmeyen kayıt görüş hedefi değil) | şeritten uzağı gören birimi olan shard'lı takım oyunu | KIT-ARCHITECTURE §10 "A8", CROSS-SHARD §8b.4 |
| A35 `MAX_SIGHT_CELLS` (4) sabit: ön-ayar yarıçapının 4 katından uzağı gören birim sıkıştırılır; oda yarıçapını büyütmek bugünkü çözüm | oda yarıçapını büyütemeyen, 4×'ten uzun görüş isteyen oyun | KIT-ARCHITECTURE §10 "A8" |
| A36 Seam ötesi 3B küre sorgusu: `Seam::within` yer düzleminde disk (`Planar`); 3B oyun bugün `Seam::area`'ya kendi küre yüklemini veriyor — hazır bir `within3` / `Spatial` sürümü | 3B seam sorgusunu sık kullanan oyun | KIT-ARCHITECTURE §10 "A5" |
| A37 `GridPartition3` için eksen başına yarı-boyut (kutu harita `[hx, hy, hz]`); bugün küp `[-half, half]³` — basık dünya `nz = 1` ya da izdüşüm ölçeğiyle kuruluyor | küp haritaya sığmayan 3B shard'lı oyun | KIT-ARCHITECTURE §10 "A5" |
| A38 Bölme ön-ayarlarında eksen başına wire ölçeği (`[f32; 2]` / `[f32; 3]`); bugün `with_wire_scale` tek ölçek, her eksende aynı | eksenleri farklı birimde nicemleyen oyun (ör. düzlem santimetre, yükseklik desimetre) | KIT-ARCHITECTURE §10 "A7" |
| A39 Shard'lı mekânsal kompozitte (`ShardedSpatialRoom`) oyuncu başına aydınlık: ödünç (şerit) kayıtların izleyicinin shard'ında entity'si yok — `lit` yalnız wire'la ya da şeride kural girdisi taşınarak | ışık konisi isteyen shard'lı AOI oyunu | KIT-ARCHITECTURE §10 "A9" |
| A40 Lit izleyicinin maliyeti: bugün her tick mahallesindeki her kayda `lit` + ayrı kodlama (`O(F·V)`); aydınlık görünümü değişmeyen izleyici için yeniden değerlendirmeyi atlamak ya da aynı ışığı paylaşan izleyicileri bir grupta toplamak | ölçüm bir oyunda `F·V`'nin adım bütçesini zorladığını gösterirse | KIT-ARCHITECTURE §10 "A9" |

### B. Taşıma ve operasyon

| Madde | Tetikleyici | Kaynak |
|---|---|---|
| B1 rUDP tıkanıklık tepkisi (pacing/hız) yok — sinyaller rUDP sertleştirme 2'de geldi (`GameEstimate`, `udp_game_*`, `udp_datagrams_dropped_kernel`); kalan **tur 3 — tepki**: talep > yol hızında kuyruk / en eskiyi düşür (sayılı) / kit-oyun sinyali kararı, raporlamayan istemci, kayıp mı gecikme mi, FRAG atomikliği, oturumlar arası adalet | rUDP'yi deneysel etiketten çıkarma (E3) + kullanıcı kararları | DESIGN §6 "Oyun bandı geri bildirimi" |
| B3 rUDP'de NAT yeniden bağlanması oturumu bitiriyor | E3 | udp/mod.rs:316, DESIGN:1219 |
| B5 rUDP kripto yok | — (v1 kapsam dışı) | udp/mod.rs:328, SECURITY:19 |
| B7 rUDP üstünde reconnect/resume e2e'si | E3 | ROADMAP:451, RECONNECT:282 |
| B8 QUIC rehome (REHOME çerçevesi, relay, 0-RTT) | çok makineli dağıtım | DISTRIBUTED:167-193 |
| B9 Süreçler arası oturum relay'i | A2 / iki makine | DISTRIBUTED:147-165 |
| B10 Süreçler arası tick senkronu | çok süreç | DISTRIBUTED:231 |
| B11 NetLink hata enjeksiyonu test donanımı | NetLink ile | DISTRIBUTED:243 |
| B14 Loadgen kapanışların istemci başına atfını yapmıyor (kod-9 mesajı sözleşmece insan-okunur; sunucu `server_closes{reason}` otorite — DESIGN §5.6 "Loadgen (B14)"); istemcinin kendi hataları t1'de sebebe göre ayrıldı (`errors_*`); yan not: "pre-auth frame budget" kapanışı `cap_rejected`'a düşüyor | istemci-başına atıf isteyen ölçüm → `Error`'a toplamalı sebep alanı | DESIGN §5.6 |
| B15 Write-stall artıkları (TLS kuyruğu 64 KiB, uyanma histerezisi, WS'te iki pencere) | — | SECURITY:117, ROADMAP:522 |
| B17 Admin HTTP: auth/TLS yok, keep-alive yok, makine-okur çıktı yok, profil yok | — (localhost sözleşmesi) | OPS:12-54, SECURITY:269 |
| ~~B20 Autobahn CI işinin ilk koşusu~~ **Push edildi (2026-09-27, kullanıcı kararı):** `main` → `origin/main` (298 commit); CI ilk kez bu yapıda koşuyor | ✅ | HANDOFF, SECURITY §3.7 |
| B21 Gerçek ticket doğrulayıcıları | — (platform tarafı) | RPC-CONTROL-PLANE:461 |
| B22 NATS/Kafka/gRPC RPC adaptörleri | — | RPC-CONTROL-PLANE:463 |
| B28 `UdpClient`'ı `gsb-net`'ten ayırmak (`gsb-client` bugün `gsb-net` üzerinden `gsb-core`'u çekiyor) | çekirdeksiz istemci derlemesi (wasm/mobil) | DESIGN §5.7 |
| B35 Loadgen RPC modu orkestre/churn'de (CLIENT satırına defter + gecikme dağılımı) ve oda-local `ABILITY` yolunun yük ölçümü | çok süreçli RPC ölçümü gerektiğinde | RPC-CONTROL-PLANE §8.2, §11 |
| B38 `metrics` fasadı exporter'ı — üçüncü `Exporter`, kendi feature'ı; aile tablosunu yürüyüp fasada basar, global recorder yalnız exporter'ın içinde (yeni crate gerektirir) | bir operatör `metrics` ekosistemini isterse | OPS §6 |
| B69 Yok edilen odanın satırı yalnız 2 rapor penceresi yaşar; 15 sn'lik kazıma son değerleri kaçırabilir — emekli oda toplamları (kümülatif, oda etiketsiz) | bir operatör isterse | DESIGN §12 |
| B70 Panikle ölen görevin içeriği sayılamıyor: kutusundakiler (gelen göçler, etkiler, op'lar), oturumlarının elindekiler, son penceresi — yalnız görev sayılıyor (`rooms_ended_uncounted`) | bilinçli sınır; bir yol bulunursa | DESIGN §12 |
| B89 rUDP kapısında kaynak adres başına oturum/el sıkışma sınırı — çerez el sıkışması durumsuz, yuva tutmaz; sınır kurulmuş ya da pre-auth oturumlar üzerinden olmalı (D11'in rUDP yarısı) | rUDP sertleştirme turu | SECURITY §4.3.1 #8 |
| B90 Ops HTTP yönlendirmesinin kendisi (`/rooms`, oda aç/kapat registry yanıtını bekler) B49'un yazma süre sınırına dahil değil — registry cevabına süre sınırı | istenirse | OPS §3 "B49" |
| B91 Okumayı bırakan istemci (ör. LEAVE'den sonra) sondalanmaya devam ediyor; `probes_unanswered` artıyor — tur 3 cevapsız sondada kadansı seyreltebilir | rUDP tur 3 | DESIGN §6 "Oyun bandı geri bildirimi" |
| B93 `GameEstimate::min_rtt` ömür boyu en küçük; gecikme temelli tepki pencereli min ister. REPORT'a bayt sayımı (sondaki eklemeli alan) | rUDP tur 3 | `udp::feedback` |
| B94 Loadgen RESULT'ında istemci tarafı rapor sayaçları (`probes_received`, `reports_sent`, `announces_sent`, `reports_send_failed`) yok | istenirse | `UdpClientStats` |
| B96 Kernel izleyicisinin `close`'ta kesildiği testlenmedi (mutasyon sağ çıktı; dinleyici tutamacı yaşadıkça saniyede bir okumaya devam eder) | düşerse | DESIGN §6 "Çekirdeğin soket kayıpları sunucuda" |

### C. Dağıtık, kalıcılık, ufuk

| Madde | Tetikleyici | Kaynak |
|---|---|---|
| C1 Kalıcılık (maç checkpoint'i; MMO kalıcılık servisi). K4'ten: stok sunucunun MMO realm'inde karakter kaynağı yok (herkes waystone 0), çıkışta konum yazılmıyor | maç: tek makine çökmesi can sıkarsa; MMO: harita gerçek oyuncuya açılırsa | PERSISTENCE:94-109, ROADMAP:657 |
| C2 Çok süreçli / çok makineli shard topolojisi (yerleşim config'i dahil) | A2, A4, B8 | DISTRIBUTED:25-40, CROSS-SHARD:670 |
| C3 Otomatik shard dengeleyici | — | DISTRIBUTED:38, 264 |
| C4 Sunucular/bölgeler arası, yük dengeleyici | — (v1 dışı) | ROADMAP:757, DESIGN:12 |
| C5 Makineler arası oyunda daha agresif crystallization | gerçek makineler arası oyun | DISTRIBUTED:258 |
| C6 Seam ötesi ortak fizik (tutma/itme) | — (v1 dışı) | CROSS-SHARD:435-440 |
| C7 Saldırı anında taze borrow (katman 1 seçeneği) | — | CROSS-SHARD:56, 665 |
| C8 Çok link'li karışık mod testi | ≥3 shard'lı düzenek | ROADMAP:347 |
| C9 Sunucu yeniden başlatmayı aşan oturumlar, bölgeler arası oturum göçü | — (kapsam dışı) | RECONNECT:26, 276 |
| C10 Yeniden başlatma sonrası toplu reconnect ölçümü (shard sayısına göre) | — | RECONNECT:385 |
| C11 Ufuk (planlanmıyor): kıtalar arası tek dünya, global sıralama, dağıtık kilit, lockstep | — | DISTRIBUTED:261, DESIGN:1675-1684 |

### D. Güvenlik

| Madde | Tetikleyici | Kaynak |
|---|---|---|
| ~~D1 Geçerli auth-sonrası girdiye saniye başı hacim sınırı~~ **KAPANDI** (E1 ile, opt-in) | ✅ | SECURITY §3.4 |
| D2 mTLS | — (ticket auth v1 için yeterli) | SECURITY:267 |
| D3 TLS 0-RTT / resumption ayarı | B8 ile | SECURITY:268 |
| D4 Admin HTTP auth/TLS | = B17 | SECURITY:269 |
| D5 rUDP kripto | = B5 | — |
| D6 Ban listesi / firewall entegrasyonu (sinyal var, tüketici yok) | — | CHANGELOG:5900 |
| D7 Registry katmanında oyun oturum politikası (reconnect'te re-auth) | — | DESIGN:1224 |
| D8 Ticket odası bağlantı ömrü boyunca sabit | — | RPC-CONTROL-PLANE:467 |
| D9 Conn tarafında RPC kapısı | — | RPC-CONTROL-PLANE:476 |
| D10 Bağlantı başına RPC geçmişi | — | RPC-CONTROL-PLANE:465 |
| D12 Düz TCP kapısının pre-auth evresinde kaynak adres başına sınır — el sıkışma evresi yok; unauthed bağlantılar registry'de sayılıyor, kaynak başına tavan orada olmalı (gsb-core; D11'in TCP yarısı) | kapı herkese açılırken ya da gözlenen saldırı | SECURITY §4.3.1 #8, §6 |

### E. Kullanıcı kararı bekleyenler (tek başına verilmez)

| Madde | Kaynak |
|---|---|
| E1 ~~Geçerli girdi hacim sınırı~~ **Karar (2026-09-27): opt-in yapı taşı** — bağlantı başına token bucket; varsayılan KAPALI, sayıyı oyun/config verir, aşan girdi düşer ve sayılır → **KAPANDI (E1 turu):** `RoomConfig::input_rate` + `input_rate_hz`/`input_burst` + `GameModule::input_rate`; SECURITY §3.4 | HANDOFF:26, ROADMAP "Ürün kararı — geçerli girdinin HACMİ" |
| E2 ~~`metrics` fasadı mı, elle render mı~~ **Karar (2026-09-27): dışa açım katmanı** — içeride ucuz toplama aynı kalır; dışa açım takılabilir exporter'lara devredilir (Prometheus mevcut, OTLP eklenir, gerekirse `metrics` fasadı), her biri feature arkasında (OPS §6'daki "dördüncü lavabo" yönü) → **KAPANDI (E2 turu):** `Exporter` dikişi + tek aile tablosu; Prometheus (`prometheus`, vars.), OTLP/HTTP itme (`otlp`, kapalı); yeni bağımlılık yok | OPS §6 |
| E3 ~~rUDP'yi deneysel'den çıkarmak~~ **Karar (2026-09-27):** şimdilik DENEYSEL kalır; hedef zamanla DTLS destekli, tam teşekküllü bir OYUN protokolü (QUIC'in yerini tutmaz: QUIC stream TCP yerine yazılmış, datagram'ı ek) — ileride bir **rUDP sertleştirme paketi** (B1–B5, B7) açılacak | ROADMAP, SECURITY:19 |
| E4 ~~Koordinat formatı~~ **Karar (2026-09-27): oyunun kararı — kapandı.** Kit her formatı taşır (`RecordCodec`, A31); demoların `sint32`'si yalnız onların seçimi; motorda iş yok | ROADMAP "Koordinat formatı" |
| E5 ~~Protokol sürüm aralığı~~ **Karar (2026-09-27): kapandı** — base protokolün geriye dönük uyumluluğu motorun sorumluluğu (varsayılan toplamalı değişiklik; uyumsuzlukta sürüm artar + N−1 geçiş desteği; ERROR 13 tespit), kabul aralığı dağıtımın config'i (`min_protocol_version`); oyunun protokol sürümü oyunun işi. Kod: İLK uyumsuz base değişikliğinde (`min_protocol_version` + N−1) | DESIGN §5 "Base protokol evrim kuralı" |
| E6 ~~AFK atılan üye odadan mı sunucudan mı~~ **Karar (2026-09-27): opt-in kapatma fiili** — oda→registry "bağlantıyı kapat" fiili; oyun/config `leave_room`/`disconnect` seçer, varsayılan bugünkü (odadan çıkar, soket açık); kapatma ERROR bildirimiyle → **KAPANDI (E6 turu):** `RoomConfig::afk_action` + oda→registry `RegistryMsg::CloseConn` + config (düz/`[rooms.<id>]`) + `GameModule::afk_action`; ERROR 9, `idle_input` | RECONNECT §16 |
| E7 ~~A22 faz 0'ın soruları~~ **Cevaplandı (2026-09-25):** kit yalnız YAPI TAŞI verir — kayıt gövdesi formatı (protobuf, MessagePack, bit paketli…), yeni zarf alanının sürümlenmesi, entity başına gönderim hızı ve istemci interpolasyonu OYUNUN kararı; kit opt-in kanca sağlar, varsayılan bugünkü davranış. Kompakt wire id: evet (herkese; istemci kuralı değişmez) | KIT-ARCHITECTURE §10 "A22" |
| E8 ~~Oyun mantığına "oyuncuyu at" fiili~~ **Karar (2026-09-27): atma = bağlantıyı kapatmak** → **KAPANDI (E8 turu):** `TickCtx::kick(player, reason)` + kit `gsb_kit::game::kick(world, entity, reason)`; üyelik `on_disconnect` ile biter, ERROR 9 `kicked: …` (gerekçe 256 bayt), `server_closes{reason="kicked"}`; yeni tel öğesi yok | RECONNECT §16.3 |
| E9 ~~Sunucu-başlatmalı "odadan çıkarıldın, bağlantı açık" bildirimi~~ **Karar (2026-09-27): gerek yok — kapandı.** Sunucunun başlattığı çıkarma bağlantıyı kapatarak yapılır (E8); istemci bunu `ERROR 9` + kapanıştan öğrenir. Yeni base kare/kod açılmaz. E6'nın `afk_action = leave_room` seçeneği (varsayılan, bugünkü davranış) aynen kalır: onu seçen dağıtımda istemci durumu ilk `ERROR 6`'dan öğrenir | RECONNECT §16 |

### F. Diğer (test, temizlik, gözlemlenebilirlik)

| Madde | Kaynak |
|---|---|
| F7 G3-2'nin gerçek düzeltmesi: one-shot full alan bağlantıya o tick grup karesini göndermemek (wire + çekirdek API) — tetik: `gap_drops`'un temiz kayıp sinyali olarak gerekmesi ya da bant ölçümü | GAME-MODULE G3-2 |
| F12 Shard'lı iki aktör odası aynı baytı göndermez (şerit/göç sırası zamanlamaya bağlı — kabul edilmiş bir tick'lik bayatlık); bayt karşılaştıran testler elle adımlanır (bilgi) | KIT-ARCHITECTURE §10 "A31" (A31-1) |
| F19 Servisler arası durdurma sırası (bir servis diğerine kapanışta yazıyorsa) — bugün hepsine istek birlikte gider | ihtiyaç doğarsa | DESIGN §9.2 elenen 5 |
| F20 Metrik örneğine global tick indisi (`RoomSample`/`RoomReport` + loadgen teli) — toplayıcının beklemesi (F29) yalnız eşit `lagged_ticks`'te iyileştirir; eşit olmayan `Lagged` sonrası tutarlı kesit için global tick indisi gerekir (bugün yırtık satıra geri düşülüp söyleniyor) | ölçüm ihtiyacı doğarsa | DESIGN §12 "tutarlı kesit" |
| F22 `input_rate_limited` için bağlantıya atıflı ilk-beş listesi (`actions_dropped_top` gibi) — bugün yalnız bağlantı başına bir `warn`; toplayıcıda bağlantı başı tablo + E2 aile tablosu | ihtiyaç görülünce | SECURITY §3.4 "Kalan yüzey" |
| F51 F35 aç bırakmasında (24 `yes` + nice 19) savaşın kararlı penceresi 64 sn'lik koşuda bile 100 adımı geçmiyor — kanıtlı koşu tavanda açık mesajla düşer (tasarım gereği); istemci tarafı (`left`, `joined`, p50) t1'de düzeldi | düşerse | CHANGELOG "t1" |
| F65 `gsb_core::channel`'da sayan, tokio'suz bir `try_send` yardımcısı yok (sonuç: Ok / Full / Closed) — "her kaybı say" için full/closed ayrımı isteyen her oyun `tokio`'yu doğrudan bağımlılık yapıp `TrySendError`'u adlandırmak zorunda (B81'de iki demo bunu yaptı) | istenirse | CHANGELOG "g1" |
| F66 `ws_going_away_unsent_{closed,stalled}` adı artık anlamıyla örtüşmüyor (B30'dan beri kapanış 1008/1013 de taşıyabilir; sayaç kodu ne olursa olsun sunucunun teardown kapanışını sayar) — yeniden adlandırma golden + loadgen teli (sihirli değer) ister | ad-anlam kuralı turunda | OPS "B80", DESIGN §5.6 |
| F67 Okuyucu pompanın beklemeli idle-timeout hükmü, duruş bildirimi kutuya önce girip bağlantı kutusunu kapattıktan sonra hâlâ slot bekliyorsa reddedilir ve hiçbir yerde sayılmaz (bağlantı yalnız kutusundakini görür); dar pencere: dolu kutu + duruş | ölçüm gösterirse | DESIGN §9 F56/F60 |
| F68 B61'in dolu dispatcher kuyruğunda `Close` kuyruğa girmez; dispatcher hükmü bilmeden düz `ConnectionClosed` ile bırakır (kayıp yok, ayrıntı yok) | bir oyun buna takılırsa | RECONNECT §3.4 |
| F69 F32 devralmasında eski canlı oturum politikaya hüküm bilinmeden `ConnectionClosed` ile sorulur (sonradan gelen `superseded` hükmü odanın muhafızında no-op) | bir oyun "devralınan" kopuşu ayırmak isterse | RECONNECT §3.3, §5 |
| F70 Toplayıcının `CUT_GRACE` sınırında yırtık çıkardığı raporlar yalnız `debug` satırı — sayılmıyor (yırtık rapor kayıp değil ama sınırın ne sıklıkla aşıldığı ölçülmeli; yeni metrik adı gerekir) | ölçüm ihtiyacı doğarsa | DESIGN §12 "Toplayıcı uçuştaki turu bekler (F29)" |
| F71 Toplayıcı odanın shard sayısını bilmiyor: ilk örneğini göndermemiş shard'ın eksik satırı (B45) beklenmiyor; `RoomSample`'a shard sayısı (tel değişikliği) eklenirse eksik satır da beklenebilir | ölçüm ihtiyacı doğarsa | DESIGN §12 "tutarlı kesit" |
| F72 Idle penceresi duvar saatidir: süreç pencereden uzun takılırsa (swap, VM duraklaması) uyanışta her canlı oturum "boşta" kapanır; pump'ın tokio zamanlayıcısı soketteki kareyi IO sürücüsü görmeden ateşleyebilir (F34'te görüldü). Seçenekler: takılma algısı (geç ateşlenen son tarihte pencereyi yeniden başlatmak) ya da olduğu gibi — **kullanıcı kararı** | üretimde toplu `idle_timeout` | CHANGELOG "t1" (loadgen_rpc) |
| F73 `loadgen_smoke`, `loadgen_games` ve `loadgen_ws` de süreç içi idle penceresine açık (loadgen_rpc'ninkiyle aynı); iddiaları pencere değilse `--idle-timeout-secs 0` | düşerse | CHANGELOG "t1" |
| F74 `loadgen_rate`'in "medyan istemci ≥ 1 snapshot" iddiası her koşuda okunur; F35 aç bırakmasında son tarihten hemen önce katılan istemciler sınırda (p50 = 1,0) — kanıtlı koşuya bağlanabilir | düşerse | CHANGELOG "t1" |
| F75 F52'nin rUDP'deki riskli gerçek saatli testleri: `udp/tests/unaccepted.rs:14` (sessizliğe dek toplam), `udp/tests/bands.rs`/`frag.rs`/`tests.rs` 200 ms–1 sn olumlu okumalar | rUDP hattının bir turunda ya da düşerse | F52 taraması |

(A6 ve F3 küçük pakette, A26 W2'de, F8, F11 ve F14 kendi turlarında kapandı; B12/B13, B19 ve B25 §B'den kendi turlarında; küçük paket 2'de B24, B26, B27, F10, F13; F9 ve B29 kendi turlarında; küçük paket 3'te B6, B16, F15, F16, F17; B31, F18, F5, B18, B23, E2, E1+F21, E6, B40, B41 ve F23 kendi turlarında; küçük paket 4'te B44, B45, F24 kapandı; F6 kendi turunda, E9 kararla kapandı; küçük paket 5'te B33, B34, B39, B46; E8 kendi turunda, yan bulgusu B48 (shard girdi-boşta saati göçte taşınmıyordu) aynı turda kapandı; B43 kendi turunda, küçük paket 6'da B37, B47; F27 ve B36 kendi turlarında; küçük paket 7'de F1, F4; sayım turunda B32, B51; B52 kendi turunda; sayım turu 2'de B53–B57; B61 kendi turunda; küçük paket 8'de B63, B64; sayım turu 3'te B58, B59, B60, B62; B65 kendi turunda; sayım turu 4'te B66, B67, B68; B71 kendi turunda; sayım turu 5'te B72, B73, B74; sayım turu 6'da B75, B80; F25 kendi turunda; B82, B83 sayım turu 7'de; F31 ve F32 kendi turlarında; F35 turunda F33, F35, F40; F41 kendi turunda; F30, F34 ve F50 aynı turda (+ tarama); F53 kendi turunda; sayım turu 8'de F54, F55 (kararla), F56, F57; B50 ölçüm turunda; temizlik paketinde A12, F2, F58, F59; B84, F61, F63 ve F62 kendi turlarında; A8, A5, A7 ve A9 kit hattının dört turunda; B81 ve F26 g1 turunda; D11 ve B49 s1 turunda; F60, F28 ve B30 c1 turunda; F29 ve F64 m1 turunda; F52 (rUDP dışı, F75 kaldı) ve B88 t1 turunda, F51 ve B14 daraldı; B85, B86, B87 rUDP hattının ikinci turunda (B1 tur 3'e daraldı); B4 ve B2 rUDP hattının ilk turunda kapandı.)

## 3. Belge bayatlıkları (tarama 2026-09-25)

İş değil, düzeltilecek metin. İşaretliler düzeltildi.

- [x] HANDOFF:66 — 10k A/B ölçümü "bekliyor" diyor; yapıldı.
- [x] ROADMAP:5-9 başlık notu — oturum zaman aşımı "yarım" diyor; kapandı.
- [x] ROADMAP:450-452 ve RECONNECT:331-335 — oda anahtarlarının `PlayerId`'ye taşınması yapıldı.
- [x] ROADMAP:639-643 — Faz B'de ✅ eksik.
- [x] DESIGN §10 — "AUTH no-op / `Authenticator`" satırı ve "Yayın = tam snapshot" satırı eskidi; :1223 olmayan §8.4'e işaret ediyor. (U turundan sonra.)
- [x] CROSS-SHARD:3-6 "ölçüm turu yürütülüyor"; :648 "v1 dışı: cross-seam etkileşim" (C1/C2 yapıldı); §9'dan sonra ikinci "## 8." başlığı; §7 başlığındaki "delta KABUL ✅" uyku notuyla çelişiyor. (D/U turlarından sonra.)
- [x] KIT-ARCHITECTURE:801, 1930 — sunucuyu oyundan bağımsız yapmak "kapsam dışı" diyor (G1 yaptı); :1733 opcode varsayılanları açık gözlem diyor (kapandı).
- [x] TICK-ARCHITECTURE:118 — "Karar gerektiren noktalar (AÇIK)"; hepsi kararlaştırıldı.
- [x] TRAIT-ARCHITECTURE:92, 133-137 — Faz 1 "BU TUR", Shard RPC ve PlayerId "yapılmadı" diyor; yapıldı.
- [x] DISTRIBUTED:217 — "(ileride) WebSocket"; WS kapısı var.
- [x] SECURITY:17 — "Autobahn yerelde koşulmadı"; koşuldu (:216-218). (U turundan sonra.)
- [x] OPS NOT-DONE'daki "çoklu-listener" — oyun taşımaları için yapıldı; admin HTTP'yi mi kastediyor, netleştir.
- [x] CHANGELOG:1087-1093 — `write_stall.rs` flaky yan bulgusu kodda düzeltilmiş; kapandı notu yok.
