# BACKLOG — iş sırası ve bırakılanlar

Bu belge, bilerek **yapılmamış** her şeyin tek yeridir. Diğer belgeler
bir maddeyi ayrıntılı anlatabilir; ama "ne açık, neden bekliyor, ne
tetikler" sorusunun cevabı burada. Bir madde kapanınca buradan silinir
ve CHANGELOG'a geçer; yeni bir erteleme doğunca buraya eklenir.

İlke (kullanıcı kararı): önce bugün yanlış olan, sonra doğrulamayı
engelleyen, en son özellik. Her iş, sebebinin yaşadığı katmana gider
("her şeyi bir yere yığmadan").

## 1. Aktif iş sırası (2026-09-25'te onaylandı)

Her paket kendi turu: ayrı worktree, alt ajan, ebeveyn doğrulaması +
bağımsız mutasyon, `main`'e fast-forward. Push yok.

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
| A5 `Grid3` (hacimsel AOI) ve `GridPartition3` presetleri | hacimsel AOI ya da 3B shard'lama isteyen oyun | KIT-ARCHITECTURE:360-368, 1297-1314 |
| A7 Bölüm presetlerinde `wire_scale` | wire'ı konum biriminden ince olan ve kabalaştıramayan oyun | KIT-ARCHITECTURE:1660, 1716 |
| A8 `Vision::sees`'te birim başına görüş yarıçapı (ward/kahraman; W2'de gerekmedi — her bölgede fraksiyon başına bir kule) | — | KIT-ARCHITECTURE:1470, 1733 |
| A9 Oyuncu başına aydınlık hücre (görünür hücrede bile ışık konisi) | — | ROADMAP:694, DESIGN:1046 |
| A10 Entity başına yayın hızı (10-15 Hz + istemci interpolasyonu; W2 kanıtı: savaş 1000'de ~365 KB/sn/istemci) | — (Unity tarafı işi) | ROADMAP:615 |
| A11 Bağlantı başına tick'te yeni `Vec` | yük verisi sorun gösterirse | ROADMAP:617 |
| A12 `QueryState`'i oda başına bir kez kurmak | — (hâlâ açık: `sharded/room.rs:220,240`, `pvs/logic.rs:193`) | ROADMAP:704 |
| A13 Registry striping (W1'den beri takım röleleri de registry'nin tek görevinde veri düzlemi yükü) | `team_export_drops > 0` ya da registry gecikmesi ölçülürse (W2: 1000'de 120 export/sn, ~281 k kayıt/sn röle, 0 düşme) | ROADMAP:700 |
| A14 Batch üstü zstd | — | ROADMAP:706, DESIGN:890 |
| A15 100k ölçeği (çok makine, congestion dahil) | — | ROADMAP:325, 364 |
| A16 RPC işçi havuzu | ~10k üyeli oda ya da oda tavanı gerçekten bağlarsa | RPC-CONTROL-PLANE:469 |
| A17 Doluluktan türetilen RPC pending tavanı | reject kovaları tavanın bağladığını gösterirse | RPC-CONTROL-PLANE:274 |
| A18 Adaptif border genişliği | — (ölçüm öncesi optimizasyon) | CROSS-SHARD:669 |
| A19 Dinamik adaptif tick hızı | — | DESIGN:1213 |
| A20 Join/leave için tick içi hızlı yol | gerekirse | DESIGN:1221, TICK-ARCHITECTURE:134 |
| A21 Dinamik oda bölme/birleştirme, bölgeler arası | — | DESIGN:892 |
| A22 Değer düzeyinde delta (kaydı son gönderilene göre göreli kodlamak; kodek seam'i + istemci çözücüleri) | hareketli iş yükünde bant ya da MTU baskısı (T: arena kazancı yalnız ~%10; W2: savaş 1000'de ~365 KB/sn/istemci) | KIT-ARCHITECTURE §10 "T sonucu" |
| A23 Demo sunucusunda `team × delta` (config şu an reddediyor; kit hazır) | demo'da takım delta'sı isteyen koşu | KIT-ARCHITECTURE §10 "T sonucu" |
| A25 Takım export'u her tick; `every k` temposu (W2 ölçtü, gerekmedi: 1000'de 0 düşme; tempo istemci baytını değiştirmez, görmeyi k tick geciktirir) | `team_export_drops > 0` ya da registry gecikmesi | CROSS-SHARD §8b.1 |
| A27 Harita geneli nötrler (ele geçirme noktası) — bugün kendi shard'ında herkese, başka yerde sisle; W2-1 kanıtı: bölüm artefaktı (shard 3 oyuncusu 640 m ötedeki sahipsiz noktayı görüyor, 100 m'deki shard 0 oyuncusu görmüyor); en küçük değişiklik varlık başına "harita geneli" bayrağı | herkesin durumunu görmesi gereken bir hedef | CROSS-SHARD §8b.5 |
| A28 Göçte ayrılan shard'daki müttefikte bir tick'lik görünürlük boşluğu (kabul; W2'de ölçülmedi — loadgen kısa kayıp-geri gelişleri saymıyor) | bir oyunda görünür titreme raporu | CROSS-SHARD §8b.5 |
| A29 Takım bütçesi üyeleri wire sırasıyla kesiyor (ilk tick'te doğan kuleler kalır, en yeni oyuncular gider) | bütçe gerçekten kesmeye başlarsa | KIT-ARCHITECTURE W2-3 |
| A24 İstemci tarafında delta uygulaması maliyeti (orkestre arena 1000'de `clients_cpu_s` +%15) | yük verisi sorun gösterirse | KIT-ARCHITECTURE §10 "T sonucu" |

### B. Taşıma ve operasyon

| Madde | Tetikleyici | Kaynak |
|---|---|---|
| B1 rUDP congestion control / pacing yok (token bucket) | rUDP'yi deneysel etiketten çıkarma (E3) | udp/mod.rs:299, DESIGN:1215 |
| B2 rUDP sabit 50 ms RTO, RTT tahmini yok | E3 | udp/mod.rs:307 |
| B3 rUDP'de NAT yeniden bağlanması oturumu bitiriyor | E3 | udp/mod.rs:316, DESIGN:1219 |
| B4 rUDP `SO_RCVBUF` ayarı yok (`socket2` zaten lock'ta, doğrudan bağımlılık gerekir) | E3 | udp/mod.rs:324 |
| B5 rUDP kripto yok | — (v1 kapsam dışı) | udp/mod.rs:328, SECURITY:19 |
| B6 Aktörü ölmüş rUDP oturumu sonraki datagrama/idle sweep'e kadar kalıyor | — | CHANGELOG:6068 |
| B7 rUDP üstünde reconnect/resume e2e'si | E3 | ROADMAP:451, RECONNECT:282 |
| B8 QUIC rehome (REHOME çerçevesi, relay, 0-RTT) | çok makineli dağıtım | DISTRIBUTED:167-193 |
| B9 Süreçler arası oturum relay'i | A2 / iki makine | DISTRIBUTED:147-165 |
| B10 Süreçler arası tick senkronu | çok süreç | DISTRIBUTED:231 |
| B11 NetLink hata enjeksiyonu test donanımı | NetLink ile | DISTRIBUTED:243 |
| B12 `stop()`'ta istemciye kapanış bildirimi yok | — | ROADMAP:710 |
| B13 `stream_rejected` kapanışlarında ERROR 9 yok | — | CHANGELOG:1303, 1466 |
| B14 Loadgen ERROR 9'u yalnız ihlal/diğer diye ayırıyor | — | CHANGELOG:1471 |
| B15 Write-stall artıkları (TLS kuyruğu 64 KiB, uyanma histerezisi, WS'te iki pencere) | — | SECURITY:117, ROADMAP:522 |
| B16 Accept döngüsü hâlâ `abort` ile duruyor | — | DESIGN:1197-1214 |
| B17 Admin HTTP: auth/TLS yok, keep-alive yok, makine-okur çıktı yok, profil yok | — (localhost sözleşmesi) | OPS:12-54, SECURITY:269 |
| B18 Oda başına config override | — | ROADMAP:712 |
| B19 `gsb-client` yardımcı crate'i (`read_frame`'in ≥6 kopyası) | — | ROADMAP:714 |
| B20 Autobahn CI işinin ilk koşusu | repo push edilince | HANDOFF:321, SECURITY:220 |
| B21 Gerçek ticket doğrulayıcıları | — (platform tarafı) | RPC-CONTROL-PLANE:461 |
| B22 NATS/Kafka/gRPC RPC adaptörleri | — | RPC-CONTROL-PLANE:463 |
| B23 Loadgen RPC trafiği modu | — | RPC-CONTROL-PLANE:479 |

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
| D1 Geçerli auth-sonrası girdiye saniye başı hacim sınırı | kullanıcı kararı (E1) | ROADMAP:576-614, HANDOFF:26 |
| D2 mTLS | — (ticket auth v1 için yeterli) | SECURITY:267 |
| D3 TLS 0-RTT / resumption ayarı | B8 ile | SECURITY:268 |
| D4 Admin HTTP auth/TLS | = B17 | SECURITY:269 |
| D5 rUDP kripto | = B5 | — |
| D6 Ban listesi / firewall entegrasyonu (sinyal var, tüketici yok) | — | CHANGELOG:5900 |
| D7 Registry katmanında oyun oturum politikası (reconnect'te re-auth) | — | DESIGN:1224 |
| D8 Ticket odası bağlantı ömrü boyunca sabit | — | RPC-CONTROL-PLANE:467 |
| D9 Conn tarafında RPC kapısı | — | RPC-CONTROL-PLANE:476 |
| D10 Bağlantı başına RPC geçmişi | — | RPC-CONTROL-PLANE:465 |

### E. Kullanıcı kararı bekleyenler (tek başına verilmez)

| Madde | Kaynak |
|---|---|
| E1 Geçerli girdi hacim sınırı — saniyede kaç aksiyon meşru (oynanış parametresi) | HANDOFF:26, ROADMAP:210, 576 |
| E2 `metrics` crate fasadı mı, elle Prometheus render'ı mı | HANDOFF:342, OPS:56 |
| E3 rUDP'yi "deneysel"den çıkarmak (kalan: B1–B5, B7) | ROADMAP:233, SECURITY:19 |
| E4 Koordinat formatı (wire `sint32` ↔ simülasyon `f32`; Unity ile) | ROADMAP:602, DESIGN:1225 |
| E5 Protokol sürüm aralığı / özellik müzakeresi (tetik: ikinci sürüm yayınlanırsa) | ROADMAP:745, DESIGN:538 |
| E6 AFK atılan üye odadan mı sunucudan mı çıkar | RECONNECT:458 |

### F. Diğer (test, temizlik, gözlemlenebilirlik)

| Madde | Kaynak |
|---|---|
| F1 `MovementSystem` birim testleri | ROADMAP:432 |
| F2 Metrik test kuyrukları (`metrics_dropped` doğru yol testleri, `bytes_out` yalnız smoke) — tetik: metrik kanalı doygunluğu görülürse | ROADMAP:391-400 |
| F4 Demo `AoiRoomExt`/`TeamRoomExt`/`SectorRoomExt`'te `with_economy` yok (sunucu `set_economy` ile dolanıyor) | GAME-MODULE:277 |
| F5 Servis yaşam döngüsünde açık durdurma protokolü yok | GAME-MODULE:931 |
| F6 Alan etkili sorgular `local ∪ borrowed`'u oyun elle birleştiriyor (kapsam notu) | ROADMAP:347 |
| F7 G3-2'nin gerçek düzeltmesi: one-shot full alan bağlantıya o tick grup karesini göndermemek (wire + çekirdek API) — tetik: `gap_drops`'un temiz kayıp sinyali olarak gerekmesi ya da bant ölçümü | GAME-MODULE G3-2 |
| F8 Admin `POST /rooms/open` oda config'ini `RoomConfig::default()` + `tick_hz`'den kuruyor, sunucu oda anahtarlarını (`max_detach_hold_secs` vb.) almıyor | RECONNECT §17 |
| F9 Crystal olayları için genel oyun/kit sayaç seam'i (W2-3: kit'in `ShardedTeamRoom::over_budget` sayacı da barındırıcıya ulaşmıyor) — tetik: canlı sunucuda operatör ihtiyacı ya da ikinci kit olay ailesi | CROSS-SHARD §4c madde 5 |
| F10 `Ticker` tick'i `std::time::Instant` ile damgalıyor — duraklatılmış tokio saatinde oyun `dt`'si ~0, yürüyen aktör testleri gerçek saatte; en küçük değişiklik `tokio::time::Instant` — tetik: gerçek saatli testler yavaşlar ya da kararsızlaşırsa | KIT-ARCHITECTURE W2-4 |

(A6 ve F3 küçük pakette, A26 W2'de kapandı.)

## 3. Belge bayatlıkları (tarama 2026-09-25)

İş değil, düzeltilecek metin. İşaretliler düzeltildi.

- [x] HANDOFF:66 — 10k A/B ölçümü "bekliyor" diyor; yapıldı.
- [x] ROADMAP:5-9 başlık notu — oturum zaman aşımı "yarım" diyor; kapandı.
- [x] ROADMAP:450-452 ve RECONNECT:331-335 — oda anahtarlarının `PlayerId`'ye taşınması yapıldı.
- [x] ROADMAP:639-643 — Faz B'de ✅ eksik.
- [ ] DESIGN §10 — "AUTH no-op / `Authenticator`" satırı ve "Yayın = tam snapshot" satırı eskidi; :1223 olmayan §8.4'e işaret ediyor. (U turundan sonra.)
- [ ] CROSS-SHARD:3-6 "ölçüm turu yürütülüyor"; :648 "v1 dışı: cross-seam etkileşim" (C1/C2 yapıldı); §9'dan sonra ikinci "## 8." başlığı; §7 başlığındaki "delta KABUL ✅" uyku notuyla çelişiyor. (D/U turlarından sonra.)
- [x] KIT-ARCHITECTURE:801, 1930 — sunucuyu oyundan bağımsız yapmak "kapsam dışı" diyor (G1 yaptı); :1733 opcode varsayılanları açık gözlem diyor (kapandı).
- [x] TICK-ARCHITECTURE:118 — "Karar gerektiren noktalar (AÇIK)"; hepsi kararlaştırıldı.
- [x] TRAIT-ARCHITECTURE:92, 133-137 — Faz 1 "BU TUR", Shard RPC ve PlayerId "yapılmadı" diyor; yapıldı.
- [x] DISTRIBUTED:217 — "(ileride) WebSocket"; WS kapısı var.
- [ ] SECURITY:17 — "Autobahn yerelde koşulmadı"; koşuldu (:216-218). (U turundan sonra.)
- [x] OPS NOT-DONE'daki "çoklu-listener" — oyun taşımaları için yapıldı; admin HTTP'yi mi kastediyor, netleştir.
- [x] CHANGELOG:1087-1093 — `write_stall.rs` flaky yan bulgusu kodda düzeltilmiş; kapandı notu yok.
