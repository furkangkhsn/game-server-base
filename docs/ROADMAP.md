# Ne kaldı? — gsb Geliştirme Listesi

> **Kod doğrulama turu:** P1 maddelerinin kodda yeniden taranmasıyla üç
> maddenin durumu güncellendi — `max_players`/`RoomFull` fiilen kapanmış
> (join yolu + ERROR 8 + shard guardrail + test), `Güvenlik yüzeyi`'nin
> iki alt parçası (`TicketAuth` hook'u, `max_connections` cap'i) kapanmış
> (yalnız post-auth aksiyon rate-limit'i açık), ~~`Oturum zaman aşımı`
> yarı kurulmuş (reader idle-timeout var; yarı-ölü bağlantı süpürmesi
> yok)~~ *(kapandı — aşağıda "Oturum zaman aşımı" [x]: idle penceresi,
> bayt-granüler yazma-tıkanma sınırı, AFK sinyali/tavanı)*. Aşağıdaki
> maddeler bu doğrulamayı yansıtıyor.

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
Test sayısı: bugün itibarıyla **952** (952/952 yeşil, 1 ignored doctest;
tarihsel ilerleme 58 → ... → 294 → 314 → 319 → 327 → 340 → 344 → 356 →
388 → 409 → 411 → 419 → 433 → 439 → 454 → 478 → 497 → 521 → 534 → 537 → 551 → 579 → 586 → 609 → 623 → 641 → 657 → 664 → 670 → 687 → 704 → 713 → 722 → 740 → 776 → 818 → 821 → 827 → 847 → 865 → 869 → 893 → 905 → 920 → 928 için `docs/CHANGELOG.md` başlığına bakınız).
Güncel iş sırası ve tüm bırakılanlar: **`docs/BACKLOG.md`**.
Son tur: **B19 — istemci yapı taşı `gsb-client`** (DESIGN §5.7) —
çerçeve, tek `Conn` (TCP/TLS/QUIC/rUDP), oturum adımları, tipli
`ServerError`; loadgen/örnek/süitlerdeki kopyalar kalktı; tel baytları ve
RESULT biçimi aynı.
Önceki tur: **A29 — takım bütçesinde oyunun sıralaması** (KIT-ARCHITECTURE
§10 "A29") — opt-in `with_export_rank`: kesmede kademe içinde oyunun
sırası (önce üyeler korunur, eşitlikte küçük wire id; varsayılan bayt
aynı); kesme `RoomSample::team_over_budget` ile barındırıcıya.
Önceki tur: **B12 + B13 — kapanış bildirimleri** (DESIGN §5.6) — `stop()`'ta
en-iyi-çaba ERROR 14 (`SERVER_STOPPING`, toplamalı kod), reddedilen akışta
ERROR 9; beklemesiz, `stop()` her zaman bitiyor; QUIC ve WS kapılarındaki
bildirim yutan iki kusur düzeldi.
Önceki tur: **F14 — düşen batch'in RPC yanıtları** (RPC-CONTROL-PLANE §3.1)
— düşen batch'in yanıtları kuyruğun başına dönüp kanalın kabul ettiği
ilk batch'le tam bir kez gidiyor; tıkalı bağlantı borcu cap'e ulaşınca
yeni istek yanıtsız reddediliyor (borç ≤ 4 + 16).
Önceki tur: **F11 — fan-out düşme sinyali** (KIT-ARCHITECTURE §10 "F11") —
çekirdek düşen batch'i (`on_batch_dropped`) ve düşme dizisinin bitişini
(`on_batch_resumed`) mantığa bildiriyor; kit tek seferlik durumu (one-shot
full tabanı, oturum yükü, ack) yeniden kuruyor, fırtınaya karşı tempolu.
Düşmeden sonra iyileşme keep-alive (~0,6 sn) yerine ~1 tick.
Önceki tur: **F8** — admin `/rooms/open` odası artık başlangıç odalarıyla
aynı şablondan (`Config::room_template`) kuruluyor.
Önceki tur: **A10 — kayıt başına yayın hızı** (KIT-ARCHITECTURE §10 "A10")
— opt-in `RecordCodec::send_every` (varsayılan her tick, bayt aynı,
istemci kuralı aynı); arena 15 Hz ile açtı: bant −43…−45 %, bayatlık ≤ 1
tick. A22 faz 0'ın motor tarafı kaldıraçlarının üçü de (A30, A31, A10)
tamam; kalan bant kararları oyunların (kodek, opt-in'ler).
Önceki tur: **A31 — paketli kayıt koşusu** (KIT-ARCHITECTURE §10 "A31") —
oyun başına opt-in (`RecordCodec::RUN`, `kit.proto` `records = 6`),
açmayanların baytı aynı; savaş demosu açtı: bant −54 %, 500'de rUDP
datagramı kare başına yarıya. Sıradaki: A10 (varlık başına yayın hızı,
opt-in kanca).
Önceki tur: **A30 — kompakt wire id** (KIT-ARCHITECTURE §10 "A30") —
shard'lar wire id'leri iç içe basıyor (`(n − 1)·N + i + 1`), istemci
kuralı aynı; shard'lı oyunlarda bant −7…−15 %, id 1–2 B. Sıradaki: A31
(paketli kayıt koşusu, opt-in yapı taşı).
Önceki tur: **A22 faz 0** (KIT-ARCHITECTURE §10 "A22") — ölçüm + tasarım,
wire değişmedi: değer-göreli kodlama şimdilik YAPILMAYACAK; kaldıraç
sırası kompakt wire id → paketli kayıt koşusu + oyun kodeği → A10.
Bakımcının beş cevabı bekleniyor (BACKLOG E7). Ölçüm aracı:
`gsb-loadgen --capture`.
Önceki tur: **W2 — doğrulama oyunu "Cephe"** (KIT-ARCHITECTURE §10 "W2
sonucu", CROSS-SHARD §8b.8) — `gsb-demo-war`: üç fraksiyon, 2×2 shard
üstünde takım sisi (müttefik harita geneli, düşman bir kulenin ya da
müttefiğin görüşüyle); `game = "war"`, `--game war`; takım sayaçları
metrik yolunda (A26). Kit değişmedi. **BACKLOG §1'in paketleri bitti;**
sıradaki iş BACKLOG §2'den.
Önceki tur: **W1 — team × sharded kompoziti** (CROSS-SHARD §8b) —
`ShardedTeamRoom` + registry hub'ı takım-export'u: müttefik harita geneli,
düşman uzak bir müttefiğin görüşüyle, başka shard'da bile. Doğrulama oyunu
(W2) sırada.
Önceki tur: **T — takım odasında delta** (KIT-ARCHITECTURE §10 "T sonucu")
— `TeamRoom::with_delta` + ortak küme defteri `common::SetLedger`; arena
delta modunda. Kazanç hareketli arena yükünde ~%10 (bulgu: değer
düzeyinde delta gerekir, BACKLOG A22).
Önceki tur: **S — çekirdek kapanış kilitlenmesi** (DESIGN §9.1) — registry
durdurma yollarında bir odanın posta kutusunu artık hiç beklemiyor
(`post_stop`: `try_send`, doluysa spawn'lu gönderici); `stop()` stop
anındaki kopuş fırtınasında asılı kalmıyor; destroy yolundaki aynı
bekleme de kapandı.
Önceki tur: **H — rUDP el sıkışma kaybı** (DESIGN §6 "El sıkışma kaybı") —
sunucu doğrulanan proof'a `ACK{1}` kabulü yolluyor, istemci onu görene
dek proof'u yeniden gönderiyor (50 ms, 5 sn sınır), sunucu proof'ta
idempotent; kademesiz 200/500 rUDP istemcinin hepsi katılıyor.
Önceki tur: **küçük paket** — oturum yükü kancası + arena `Welcome` (G3-3
kapandı), G3-2 belgelendi, loadgen CLI hataları, rustdoc CI kapısı,
`max_detach_hold_secs`, zorlanmış bekletme / uzak etki / göç sayaçları.
Önceki tur: **U — rUDP parçalama** (DESIGN §6 "MTU") — oyun bandında
bütçeyi aşan kare FRAG ile bölünüp istemcide birleşiyor; G3-1 ve
kümelenmiş MMO taşması taşımada kapandı (yazıcı düşürmeleri → 0). Yan
bulgular BACKLOG §1'de: rUDP el sıkışma proof kaybı, çekirdek kapanış
kilitlenmesi.
Önceki tur: **K4 — oyuncu kimliği → ev shard'ı** (GAME-MODULE "K4
sonucu") — çekirdek yönlendiriciye ve join kancasına doğrulanmış kimliği
veriyor (`HomeShard`, `on_join_as`), kit `spawn_player_as`'a iletiyor,
MMO realm'i onunla anahtarlı; gerçek istemcilerle MMO yükü dört shard'a
yayılıyor (200 bot: 53,50,50,47), loadgen'in ilk-`Travel` hilesi yok.
Önceki tur: **D — göç tick'i** (CROSS-SHARD §4d) — göç eden entity'nin eski
shard'daki ölümlü kopyasına `h + 1`'de inen yerel darbe artık yeni sahibe
yönleniyor ve bir kez uygulanıyor (kopya oyun kancalarından gizli, yeni
sahibin ödünç kaydı); kopya ikinci kez davranmıyor. Çekirdekte tek ekleme
(`CrossSeam::departed`).
Önceki tur: **cross-seam C2** (CROSS-SHARD §4c) — seam ötesi süren dövüş
tek shard'a kristalleşiyor (yüksek wire alçak wire'ın shard'ına mevcut
göçle; sahiplik dövüş sürerken bölgeden ayrışık; zaman + yarım-bant
histerezisi); MMO açık, loadgen `--mmo-duel-frac`. **Cross-seam
etkileşim paketi bitti.** Yan bulgu (kümelenmiş MMO `snap_overflows`)
U turunda taşımada kapandı.
Önceki tur: **cross-seam C1** (CROSS-SHARD §4b) — oynanış seam'in
ötesini görüyor (`CrossSeam`/`Seam`) ve `ShardMsg::RemoteEffect` ile
etkiliyor (idempotent, sıralı, yönlendirmeli); MMO saldırısı seam
ötesinde. Sıradaki: crystallization.
Önceki tur: **oyun modülü G3** (`docs/GAME-MODULE.md` §5 "G3 sonucu") —
loadgen `--game demo|arena|mmo`; demo botu birebir; arena ve MMO ilk yük
tabanları (1000 istemci orkestre, 30 Hz, adım p50 1,6 ms / 0,29 ms).
**Oyun modülü fazları (G1–G4) bitti.** Açık: G3-1 (arena full'ları rUDP
MTU'sunu aşıyor), G3-2 (join'de delta'lar private full'dan önce), G3-3
(arena istemcisi takımını wire'dan öğrenemiyor), K4 (kayıtlı karakter
oturuma bağlı).
Önceki tur: **oyun modülü G4** (`docs/GAME-MODULE.md` §5 "G4 sonucu") —
kit'in istemci kuralları `gsb_kit::client`'ta; altı kopya ona geçti;
demo tarafındaki `CellExit` okuma hatası (G4-1) düzeltildi; alıcı döngü
eskisinden hızlı. G3'ten önce koşuldu.
Önceki tur: **kit düzeltme turu** (`docs/GAME-MODULE.md` §5 "Kit düzeltme
turu") — G2 bulguları K1–K3 (oyuncunun girdi oturumu artık `KitMig`
içinde göçle taşınıyor) ve K5 (örnek config'in demo anahtarları yorumda)
kapandı; CI her oyun özelliğini tek başına derliyor. Açık bulgu: K4
(kayıtlı karakter oturuma bağlı; MMO yükü shard 0'da başlar).
Önceki tur: **oyun modülü G2** (`docs/GAME-MODULE.md` §5 "G2 sonucu") —
arena ve MMO sunucuda (`game = "arena" | "mmo"`, `[arena]` / `[mmo]`
tabloları, sabit anahtarlar başlatmada reddediliyor), gerçek soketlerle
uçtan uca testli; birden fazla MMO dünyası çalışıyor.
Önceki tur: **oyun modülü G1** (`docs/GAME-MODULE.md`) — sunucu oyunu
nesne-güvenli `GameModule` seam'i arkasında barındırıyor; 2D demo ilk
modül (`game = "demo"`), oyunsuz derleme CI kapısı, RESULT'ta `game=`,
orkestratör `--cell-size` ve ekonomi asimetrisi düzeltildi. Sıradaki:
G2 (arena + MMO modülleri, oyun adını taşıyan config tablosu).
Önceki tur: **süreli bekletmede veto + veto tavanı** (`docs/RECONNECT.md`
§17) — `may_release` artık süreli bekletmenin deadline'ında da soruluyor
(veto uzatır, her tick yeniden sorulur), duran veto yeni
`RoomConfig::max_detach_hold` (varsayılan 10 dk, kopuştan itibaren;
süresiz bekletmeye de uygulanır) ile sınırlı; veto etmeyen oyunda
davranış aynı. MMO "çıkış sayacı + savaşta çıkış yok"a geçti.
Önceki tur: **WS uyum kapısı** (`docs/SECURITY.md` §3.7): el yazımı RFC
6455 okuyucusu §5 ve §7'ye karşı kural kural denetlendi. Bilinen açık
kapandı: açık parçalı mesajın içindeki yeni veri çerçevesi yarım mesajı
sessizce atıyor ya da içine teslim ediliyordu, artık 1002. Denetimin
bulduğu üç sapma da kapandı: MSB'li 64-bit uzunluk 1009 yerine 1002,
minimal olmayan uzunluk 1002, gönderilemez kapanış kodu 1002 ve UTF-8
olmayan sebep 1007. Her kural okuyucu seviyesinde kilitli (+24 test).
CI'da Autobahn fuzzing client işi var; hedefi üretim kapısı + opak mesaj
eşlemesi. Text vakaları sözleşme gereği gerekçeli olarak dışlandı. İş
yerelde gerçek imajla (`crossbario/autobahn-testsuite:25.10.1`) koşuldu:
98 vaka, 96 OK + 2 INFORMATIONAL, denetleyici geçti (ilk yazılan `0.8.2`
etiketi yoktu, düzeltildi). Önceki tur: **gsb-kit Faz 5** (`docs/KIT-ARCHITECTURE.md` §10 "Faz 5
sonucu") — **kit düzeltme turu**: iki kontrol demosunun kaydettiği
bulguların hepsi kit'te kapandı, her biri kendi commit'inde ve önce
kırılan testiyle — F1 (doğruluk: sharded × spatial'da ödünç hücresine
göç eden entity artık silinmiyor), F2 (`GridPartition2::with_diagonals`,
MMO kullanıyor), F3 (`Planar` birim sözleşmesi + debug denetimi), F4
(`with_disconnect_policy` + `Game::may_release`; varsayılan aynı, MMO
çıkış sayacına geçti), A1 (`TeamGame::spawn_team_player`, arena
`HomeBase`'i bıraktı), A2 (istemci kuralları `kit.proto`'da), A3
(opcode sabitleri varsayılansız — oyun yazarı için kırıcı). Çekirdek ve
baytlar dokunulmadı; kapanış kontrolü 83/83; loadgen gürültü içinde.
**Kit tasarımının kabul kriterinin dördü de sağlandı** (§11). Önceki
tur: **gsb-kit Faz 4** (`docs/KIT-ARCHITECTURE.md` §10 "Faz 4
sonucu") — **3D MMO demosu** `gsb-demo-mmo`, kapanış doğrulaması: yalnız
kit'in public yüzeyiyle shard'lı dünya üzerinde yer-düzlemi ızgara AOI
(`ShardedSpatialRoom<MmoGame, GridPartition2<Pos3>, Grid2>`, 2×2 shard,
64 m hücre, `Planar` = `[x, z]`), oyun kodunun spawn/despawn ettiği ve
hızsız göç eden mob'lar, desimetre nicemlemeli kodek (konum + tür +
can), park → AI devri, `mmo.proto` (opcode'lar 1200..=1204); 24 test
(12'si gerçek dört shard aktörü üzerinden, mutation-check'li).
**Beş korunan crate'e dokunulmadı**; üç demo aynı kit üzerinde yeşil
(82/82). Dört tasarım bulgusu kayıtlı — biri **doğruluk hatası** (F1:
sharded × spatial'da şeritten ödünç verilmiş hücresine göç eden entity
yeni shard'ında siliniyor), köşegen şerit yok (F2), `Planar` birim
sözleşmesi (F3), park sonu seçilemiyor (F4) — hepsi Faz 5'te kapandı.
Önceki tur: **gsb-kit Faz 3** (`docs/KIT-ARCHITECTURE.md` §10 "Faz 3
sonucu") — **3D arena demosu** `gsb-demo-arena`, kit'in kabul testi:
yalnız kit'in public yüzeyiyle yazılmış 3D takım sisi (yükseklik
sayılır, üç takım, `TeamRoom<ArenaGame, VisionGrid3<Pos3>>`, yarıçap
15 m), kendi 3D hareketi, santimetre nicemlemeli kodeği, katılım
sırasıyla round-robin takım ataması, `arena.proto` (kit zarfının tipli
aynası, opcode'lar 1100..=1102); 15 test (7'si gerçek oda aktörü
üzerinden, mutation-check'li). **`gsb-kit` ve `gsb-core`'a dokunulmadı**;
iki engelleyici olmayan tasarım bulgusu kayıtlı (takım spawn'dan sonra
soruluyor; kit zarfının istemci kuralları demo'nun proto'sunda).
Önceki tur: **gsb-kit Faz 2** (`docs/KIT-ARCHITECTURE.md` §5.1, §10 "Faz 2
sonucu") — **crate bölmesi**: `gsb-game` → `gsb-demo` (örnek oyun) ve
yeni `gsb-kit` (stratejiler, delta motoru, sharded kompozitler,
park/resume, ön-ayarlar, kendi `kit.proto`'su — `Private`'a oyunun özel
yükü için `bytes game = 4`); kit hiçbir profilde demo'ya bağlı değil
(katman testi emekli, yerine cargo döngüsü + kit manifest testi); kit
testleri kendi fikstür oyununda, demo kodeğinin değerine bakan 11 test
demo'da; demo kurucuları uzantı trait'leri (`gsb_demo::prelude`); kit
ile demo'nun tipli aynası crate sınırında aynı baytlara kilitli;
arenanın 3D ön-ayarı `Spatial` + `VisionGrid3` kuruldu (`Grid3` /
`GridPartition3` tetikleyici bekliyor); wire baytları ve public yollar
aynı, loadgen gürültü içinde. Daha önce: **gsb-kit Faz 1b** (§4.6, §10 "Faz 1b
sonucu") — **Faz 1 bitti**: takım sisi, PVS ve iki sharded kompozit
oyun üzerinden generic (`Vision`, `SectorMap`, `Partition`, `TeamGame`,
`ShardGame`, `KitMig`; 2D ön-ayarlar `VisionGrid2`, `ConvexSectors2`,
`GridPartition2`), ön-ayarlar oyunun tiplerini `Planar` erişimcisiyle
okuyor, sharded park kopyası birleşti, §8.2–§8.5 açıklarının hepsi önce
testle kanıtlanıp kapandı, bütün kit odaları istekleri oyuna
yönlendiriyor, seam kit zarfına indi. Daha önce:
**gsb-kit Faz 1a** (`docs/KIT-ARCHITECTURE.md` §4.5, §10 "Faz 1a sonucu") — §4
seam trait'leri (`RecordCodec`, `CellSpace` + `Grid2`, `Game`) kuruldu,
hücre-delta motoru oyunun wire değeri üzerinden generic oldu,
`OpenRoom<G>` ve `AoiRoom<G, S>` çevrildi, `WireId`'nin tek inşa yolu
kit'in `Minter`'ı (§8.1 kapandı), `OpenRoom`'un değişiklik penceresi
açığı kanıtlanıp kapandı (§8.3'ün o odaya düşen kısmı); wire baytları
ve public yollar aynı, loadgen gürültü içinde. Daha önce: **gsb-kit Faz 0** — `gsb-game/src` `kit/` ve
`demo/` olarak bölündü, kit'in demo'ya her erişimi geçici
`kit/seam.rs`'ten geçiyor ve kural bir kaynak-tarayan testle kilitli.
Daha önce: **stall
gözlemlenebilirliği + bayt-granüler ilerleme** — 10k ölçümünde sunucu
4486 oturumu write stall ile kapatırken `RESULT` `errors=0` diyordu.
Sunucunun başlattığı her kapanış artık sebebiyle sayılıyor
(`gsb_net_server_closes_total{reason}`, loadgen `server_closes=`), ve
stall saati kare tamamlanmasını değil soketin kabul ettiği BAYTI
ölçüyor (pencereden uzun süren bir kareyi okuyan yavaş istemci artık
öldürülmüyor); yan bulgu olarak WS kapısının kuyruk uyandırması
düzeltildi. 10k A/B ölçümü alındı; açık kalan `outbound_dead` yanlış atfı da kapandı (writer pump hüküm için doğumda bir mailbox slotu ayırıyor — CHANGELOG "Küçük düzeltme paketi"). Daha önce:
**metrik fold denetimi** — sharded oda raporunu tek satıra katlayan
`fold_rooms` alan alan değil toplu denetlendi. Her alanın katlama
kuralı kararlaştırılıp koda yazıldı ve kural YAPISAL hâle getirildi
(tam destructure → yeni alan derlemiyor). Daha önceki tur: **AFK sinyali +
girdi-boşta tavanı** — bekleyen dört ürün kararından üçüncüsü kapandı
(`TickCtx::since_input` sinyali + varsayılan KAPALI
`max_idle_input_secs` tavanı). Açık kalan tek ürün kararı: geçerli
girdiye hacim limiti.
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
"Kapatılanlar (supervision turu)"). rUDP REL bandının sessiz give-up'ı o turda
**bilinçli olarak ertelenmişti** (taşıma deneysel statüye alındı); en
sonki **rUDP doğruluk turu** onu ve cookie'nin tekrar-oynatılabilirliğini
kapattı (aşağıda, "Kapatılanlar (rUDP doğruluk turu)") — taşıma yine de
DENEYSEL: mezuniyet ürün kararı, kalan iş `crates/gsb-net/src/udp/mod.rs`
"What is still open" başlığında madde madde doğrulandı. En son **tablo budama
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
- [x] **Cross-seam etkileşim paketi** — in-process sharding'i "neredeyse
  invasif olmayan" seviyeye taşıyan üç parça (dış danışma diyaloğundan;
  tasarım notu CROSS-SHARD §2–§4 + bu madde):

  1. ~~**Ödünç kayıtların gameplay'e açılması**~~ — **KAPANDI (C1)**:
     `CrossSeam` / kit `Seam` (`lent`, `lent_iter`, `local`), yerinde
     okuma, own wins (CROSS-SHARD §4b).
  2. ~~**`RemoteEffect` primitifi**~~ — **KAPANDI (C1)**: yönlendirme,
     köken başına kayan dedup penceresi, sınırlı yeniden deneme,
     `(source, origin, seq)` + bir tick hizalama; MMO `Attack` seam
     ötesinde (CROSS-SHARD §4b, sekiz sapma gerekçeli).
  3. ~~**Crystallization tetikleyicisi (histeresizli)**~~ — **KAPANDI
     (C2)**: iki yönlü K-tick serisi, yüksek wire taşınır, pin'le
     sahiplik bölgeden ayrışık, `release` sessizlik + `Partition::holds`
     bandı (giriş yarım bant) (CROSS-SHARD §4c).

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
- [x] **Kalan metrik sayaçları için doğru-yol testleri** — kapatıldı
  (CHANGELOG "sayaç envanteri kapanış turu"). Maddenin kendi listesi
  BAYATTI ve tur ona güvenmedi: envanter sıfırdan yeniden türetildi
  (metrik yüzeyine ulaşan her tip alan alan tarandı), sonra kapatıldı.
  Test 361 → **388**; 46 mutasyonun tamamı öldürüldü, kırılamayan test
  yok. Kapananlar: RoomSample'ın `lagged_*`, `keepalive_resends` (oda
  tarafı), `step_fine_hist` (oda tarafı), `snap_bytes*`,
  `snap_overflows`, `snap_records`, `shipped_*`, `private_frames`,
  `leaves`, `requests_local/external/timed_out`, `pending_requests`
  (oda tarafı), `metrics_dropped`; **RegistrySample'ın tamamı**
  (`rooms`, `rooms_created/destroyed/died`, `joins/leaves`,
  `opens/closes` — bu tip turdan önce tek bir testi bile yoktu);
  ConnSample'ın `frames_in/out`, `violations`, `last`;
  UdpClientStats'ın dördü (`retrans_out`, `dup_in`, `oob_dropped`,
  `gave_up`). Tam ÖNCE/SONRA tablosu CHANGELOG §2'de.
  Yöntem notu (sonraki turlar için): bir alanın YALNIZCA sıfır olduğunu
  assert eden test, hiç yazılmayan bir alandan ayırt edilemez —
  `requests_timed_out` tam olarak o durumdaydı. Ve testler sayacı
  "arttı mı" diye değil, **komşusundan ayırt ederek** yazıldı (tepe ≠
  sonuncu, akış ≠ gauge, ölüm ≠ destroy, ihlal ≠ trafik, kadans ≠
  kayıp); bu depoda bulunan hatalar hep yanlış kola bağlı sayaçlardı.

  **Açık kalan kuyruk (bilinçli):** `RegistrySample::metrics_dropped` ve
  `ConnSample::metrics_dropped` — mekanizma üç üreticide de aynı ve oda
  tarafında kapatıldı, kalan ikisi aynı desenin kopyaları; ikisi de
  tasarım gereği zararsız (sayaçlar kümülatif, sonraki örnek her şeyi
  taşır) ve ikisini sürmek oda sürümünden belirgin biçimde daha
  kırılgan testler gerektirirdi (registry olay-tetikli örnekler; conn
  aktörü en fazla `METRICS_FLUSH_EVERY`de bir flush'lar). Ayrıca
  `ConnSample::bytes_out` hâlâ smoke düzeyinde (`frames_out` tam
  kapandığı için gerileme riski düşük). Bunlar tetikleyici beklesin:
  bir metrik kanalı doyması gerçekten gözlenirse kapatılır.

- [x] **`step_min_us` / `late_min_us` minimum DEĞİL** — kapatıldı
  (CHANGELOG "minimum sayaçlar turu"). Kullanıcı kararı **ONARIM**:
  alanlar gerçek minimum yapıldı; `step_first_us` diye yeniden adlandırma
  ELENDİ (yanındaki `step_max_us` gerçek bir maksimum — aynı satırda
  farklı anlama gelen bir "min" her okuyucuyu yanıltır). Muhasebe iki
  aktörden `RoomCounters::observe_late_us`/`observe_step_us` çocuk
  modülüne alındı; ilk gözlemin iki ucu da SEED etmesi korundu (sıfırdan
  başlayan bir minimum sonsuza dek 0 kalırdı). Aynı turda **ikinci, gizli
  örnek** de kapandı: `loadgen::report::fold_rooms` `late_min_us`'i hiç
  katlamıyordu, yani sharded oda shard 0'ın değerini raporluyordu.
  Loadgen kanıtı: TCP `step_min_us` 264 → **9** (`step_p50_fine_us=40`).
  Bununla birlikte aşağıdaki "doğru-yol testi" maddesinin
  `step_min/sum_us` ve `late_*` satırları da kapandı.
- [x] **`fold_rooms` eksik katlıyor (minimumlar dışında)** — kapatıldı
  (CHANGELOG "metrik fold denetimi turu"). Toplu denetim yapıldı: her
  alan için kural kararlaştırıldı (SUM / MIN / MAX / adım-ağırlıklı
  ortalama / bölünmüş gauge / KATLANMAZ) ve kural **koda**, onu
  uygulayan tek döngünün yanına yazıldı; tablo DESIGN §12'ye de işlendi.
  Yapısal koruma: döngü `RoomReport`'u tam destructure ediyor, yani yeni
  bir alan E0027 ile derlemeyi kırıyor — kural artık derleme zamanında.
  Maddedeki alanların hepsi düzeldi (`late_mean_us` ağırlıklı ortalama,
  `budget_us` MIN, `req_*` + `metrics_dropped` + `pending_requests` SUM,
  üç `*_s` oranı SUM). Denetim ayrıca iki alan-dışı hata buldu:
  akümülatör shard 0'ı HER toplamda iki kez sayıyordu (50 istemcilik
  sharded koşu `members=61` diyordu) ve loadgen'in ince-histogram
  percentilleri nüfus olarak `steps`'i veriyordu (`steps` MAX,
  histogramlar SUM ile katlanır → 4 shard'da "p50" kabaca p12.5 idi).
  `detached` da MAX'tan SUM'a alındı: komşuları `members`/`groups` ile
  aynı cinsten bölünmüş bir gauge.

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
  attempt`. Kalan not: ~~oda içi anahtarların uzun vadede PlayerId'ye
  taşınması (RECONNECT §14.1)~~ *(kapandı — TRAIT-ARCHITECTURE §6 Faz 2,
  `1c93477`: oda tabloları `PlayerId` anahtarlı, RebindKey tek bağlama
  satırına küçüldü)*; rUDP üstünde e2e varyantı (deneysel statüye
  bağlı).
  Sonraki tur: süreli bekletmede `may_release` vetosu + mutlak veto
  tavanı `max_detach_hold` (harass-lock çekirdekte sınırlı, §17). Açık:
  zorla bitirilen bekletmeler için `RoomSample` sayacı; sunucu
  config'inde `max_detach_hold` ayarı.

- [x] **Oturum zaman aşımı** — *kapandı: idle penceresi, yazma-tıkanma
  sınırı (bayt ilerlemesi) ve AFK sinyali/tavanı — iki ürün kararı da
  aşağıda kapandı.* Reader pump'un idle zaman aşımı (`idle_timeout_secs`,
  vars. 30 sn) **istemci sessiz** durumunu yakalıyor: hiçbir frame
  gelmezse pump `ConnIn::ServerClosed` gönderir, teardown kaskadı
  (registry → oda `Detach`) çalışır.

  **DÜZELTME (teknik borç turu, madde b).** Bu maddenin eski metni
  boşluğu yanlış tarif ediyordu: *"istemci hâlâ heartbeat atıyor ama
  karşı taraf gitmiş (RST'siz kopma)"*. Bu durum tutarsız — sunucu
  açısından **istemci zaten karşı taraftır**; heartbeat frame'leri
  gelmeye devam ediyorsa yol açık, istemcinin TCP yığını canlı ve
  uygulaması heartbeat zamanlayıcısını çalıştıracak kadar ayakta
  demektir. Önerilen çözüm de (heartbeat son-görülme damgası +
  süpürme) bir **no-op**'tu: heartbeat, mevcut idle penceresini
  sıfırlayan şeyin ta kendisi (`pump.rs` her okumayı yeniden sarar),
  dolayısıyla aynı olayı ikinci bir saate damgalamak birincisinin
  görmediği hiçbir şeyi görmez. Phase 0c de "tüm bağlantıları gezen"
  bir döngü değil — yalnızca `detached` satırlara bakar, canlı bir
  bağlantıyı hiç görmez.

  Gerçek boşluk **soketin diğer yarısındaydı** ve üç katman birden
  görmezden geliyordu: writer pump bir yazma/flush hatasında çıkar ama
  elinde inbox yoktur (sessiz çıkış); oda fan-out'u
  `TrySendError::Closed`'ı `Full` ile aynı sayar (batch'i sakla, gelecek
  tick tekrar dene — sonsuza kadar); bağlantı aktörü de gönderim
  sonucunu atıyordu (`let _ = self.out.send(..)`). Sonuç: bir daha tek
  bayt alamayacak bir oturum, registry satırını ve `max_players`
  slot'unu tutmaya devam ediyordu — varsayılan kurulumda reader'ın 30
  sn'lik penceresi sonunda süpürene kadar, `idle_timeout_secs = 0`
  (pencereyi kapatan, desteklenen ayar) ile **sonsuza kadar**.
  - [x] **Kapandı:** kapalı out kanalı artık bağlantıyı düşürüyor
    (`w_closing`, mevcut `v_closing`/`p_closing` deseni; yeni mesaj
    sınıfı, süpürme, zamanlayıcı ya da tick gövdesine await YOK).
    Kilit: `tests/half_dead.rs`.
  - [x] **Kapandı — tıkanmış yazma (bağlantı sınırları turu).**
    Kullanıcı kararı: eşik **süre** cinsinden, ve ölçü **İLERLEME**
    (yaş değil) — `write_stall_secs`, varsayılan 10 sn, `0` kapatır,
    `idle_timeout_secs`'in sözleşmesiyle birebir. Soketine bu süre
    boyunca hiçbir şey başarıyla yazılamamış bağlantı olağan teardown'la
    kapanır. "Yavaş istemci tolere edilir" sözleşmesiyle çelişmiyor,
    çünkü geride kalan ama hâlâ boşaltan istemcinin her tamamlanan
    yazması saati yeniden başlatır; düşen snapshot'ları eskisi gibi
    sayılır. Saat writer pump'un yazma deadline'ında (reader'ın
    idiomunun aynısı), verdict aktörün mailbox'ından gider ve
    `sink.close()` çağrılmaz — teardown, tıkanmış olanın bir bayt daha
    kabul etmesini gerektirmez. Kilitler: `gsb-net`
    `tcp::tests::stall` (gerçek loopback, hiç okumayan peer + tersi) ve
    `write_stall.rs` (uçtan uca: oturum biter, registry satırı bırakılır
    — QUIC kapısından, çünkü alım penceresini İSTEMCİ ayarlar, yani
    koşul megabayt yerine kilobaytla zorlanır). SECURITY §3.5.

    Turda çıkan yan bulgu: kanal dolduğunda aktörün kendisi de
    `send_frame`'de park ediyor, inbox doluyor ve reader `in_tx.send`'de
    parkediyordu — yani idle deadline'ı ARTIK KURULMUYORDU bile. Yeni
    saat bu düğümün dışındadır (writer'ın kendi görevindedir).
  - [x] **Kapandı — saat baytı ölçüyor + kapanışlar sayılıyor (stall
    gözlemlenebilirliği turu).** İlk uygulama saati bir karenin
    gönderimi BÜTÜN OLARAK tamamlanınca sıfırlıyordu: pencereden uzun
    sürede boşalan bir kareyi okuyan istemci öldürülüyordu (10k
    ölçümü: 4486 öldürme, ~80 KB kareler) ve bu öldürmeler hiçbir
    sayaçta görünmüyordu (`errors=0`). Artık soketin kabul ettiği her
    bayt saati yeniden başlatır (`WriteProgress`), ve sunucunun
    başlattığı her kapanış sebep bazında sayılır (SECURITY §3.6, DESIGN
    §12). Kalan kalıntılar (TLS kuyruğu ≤64 KiB, çekirdek uyanma
    histerezi, WS'de iki pencereye kadar sınır) SECURITY §3.5'te.
    10k A/B ölçümü alındı (CHANGELOG "Ölçüm kaydı"; commit'ler e-posta
    düzeltmesinden sonra `c2b7160` ↔ `d8d9030`): fark gürültü içinde —
    kesilenler yavaş-ama-okuyan değil, hiç okumayan istemcilerdi.
    Aşırı yükteki `outbound_dead` yanlış atfı (koşu başına 67–172) da
    kapandı: stall hükmü doğumda ayrılmış mailbox slotuna gidiyor
    (SECURITY §3.5 karar 4).
  - [x] **Ürün kararı 2 — AFK/zombi oturum — KAPANDI** (CHANGELOG
    "AFK sinyali + girdi-boşta tavanı turu"). Karar: AFK'nın KENDİSİ
    oyunun kararıdır, base ona bir SİNYAL verir ve bir TAVAN sunar.
    - **Sinyal (koşulsuz):** `IdleClock` her üyenin son *aksiyon taşıyan*
      karesini tutar; oyun mantığı `TickCtx::since_input(player)` ile
      okur. Tanım YAPISAL: bağlantı aktörünün odaya `Action` olarak
      ilettiği kare (kayıtlı game-band opcode + base-band RPC zarfı).
      HEARTBEAT bağlantı aktöründe yanıtlanır, odaya hiç ulaşmaz — yani
      trafik penceresi ile etkinlik penceresi artık AYRI saatlerdir ve
      `e2e.rs::active_heartbeat_survives_the_idle_window` **hiç
      değişmedi** (canlılık sözleşmesi aynen duruyor).
    - **Tavan (varsayılan KAPALI):** `max_idle_input_secs`. Açıkken
      süresi dolan üye, ölü taşımanın gittiği AYNI yola verilir
      (`on_disconnect`) — park / AI devri / despawn kararını oyun verir,
      base kendiliğinden despawn etmez. Kapasite emniyet supabı, politika
      değil.
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
  `max(cap/4, 64)`, SECURITY §4).

  **Post-auth girdi (teknik borç turu, madde c — ölçüldü ve bölündü).**
  Üç pre-auth limitinin üçü de `ConnState::WaitingAuth`'a bağlı, yani
  auth sonrası tamamen kapanıyor. Kalan maruziyet iki ayrı şeydi:
  - [x] **Çöp opcode bedavaydı — kapandı.** Bilinmeyen *base-band*
    opcode hard ihlal sayılırken, tabloda tanımsız bir *game-band*
    opcode körlemesine iletiliyor, odanın tick bütçesinden çekiliyor ve
    ancak oyunun ingest'inde sessizce atılıyordu: cevap yok, puan yok,
    sınır yok. Mesaj tablosu sunucunun tel sözleşmesi olduğuna göre
    (DESIGN §5) tanımsız opcode artık base-band'deki kardeşiyle aynı
    hard ihlal — mevcut ağırlıklı ömür bütçesi, yeni mekanizma yok.
    Kilit: `violation.rs::undefined_game_band_opcode_is_a_hard_violation`
    + tersi `registered_game_band_opcode_keeps_its_race_class`.
  - [ ] **Ürün kararı — geçerli girdinin HACMİ.** Per-tick çekme bütçesi
    (16/bağlantı/tick ≈ 480/sn) **odayı** sınırlar, istemcinin gönderme
    hızını değil; bütçeyi aşan girdi göndericinin kendi bounded
    kanalında birikir ve taşarsa **kendi** girdisini düşürür — sayılıp
    ona atfedilerek (`gsb_net_actions_dropped_total` +
    `actions_dropped_top`). Yani hasar kendine dönüktür ve zaten
    ölçülür. Bunun üstüne bir hız limiti koymak bir *sayı* seçmek
    demektir (saniyede kaç aksiyon meşru?) — bu bir oynanış
    parametresidir, ve SECURITY §3'ün pre-auth heartbeat'leri
    bilerek bütçelememe gerekçesiyle (dürüst-ama-hatalı istemciyi
    zorla düşürmek) aynı riski taşır.
  - [x] **Kapandı — post-auth HEARTBEAT_ACK amplifikasyonu (bağlantı
    sınırları turu).** Kullanıcı kararı: aynı §3.2 eşiği auth sınırının
    ötesine taşındı (yeni makine yok). Semantik endişesi ölçüldü ve
    geçersiz çıktı: düzgün bir istemci saniyede bir heartbeat atar, yani
    eşiğin kendi temposundadır — her cevabını ve `HeartbeatAck.tick`'ten
    okuduğu RTT'yi olduğu gibi alır. Saat tek (auth başarısında BİR
    sıfırlama: authenticated oturumun ilk heartbeat'i her zaman
    cevaplanır), sayaç iki (pre-auth fazlalık güvenlik sinyali,
    post-auth fazlalık istemci-kalitesi sinyali — birleştirmek
    §3.2'nin sayacına kendi sorusunu yanıtlatamaz hale getirirdi).
    Fazlalık bilerek bütçeye YAZILMIYOR (pre-auth gerekçesinin aynısı).
    `active_heartbeat_survives_the_idle_window` gevşetilmedi,
    GÜÇLENDİRİLDİ: artık kısma ile idle penceresinin dikişini pinliyor
    (penceresini sıfırlayan frame'in GELMESİ, cevabı değil) ve ack
    sayısını iki taraftan da doğruluyor. SECURITY §3.2.
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
  başına girdi rate-limit — çöp opcode tarafı kapandı, geçerli girdinin
  hacmi ürün kararı olarak açık (güvenlik maddesi).
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
  - **Faz B:** ✅ Kapatıldı (`76777c2`; bugün kit'te
    `ShardedSpatialRoom<G, P, S>`) — `sharded × spatial` kompoziti —
    her shard kendi içinde hücre-gruplu yayın yapar (AoiRoom mantığının shard-içi örneklanması);
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

- [x] **Kapanma bildirimi** — **KAPANDI** (B12): `stop()`'ta
  istemci en-iyi-çaba ERROR 14 (`SERVER_STOPPING`) alır, kapının
  sonundan önce; beş kapıda testli (DESIGN §5.6).
- [ ] **Oda bazlı config override** — factory aynı config'i kullanıyor;
  yüksek yoğunluklu odalar için farklılaşma.
- [x] **`gsb-client` yardımcı crate'i** — **KAPANDI** (B19): çerçeve +
  tek `Conn` + oturum adımları + tipli ERROR tek crate'te; loadgen, örnek
  istemci ve sunucu süitleri ona geçti; kopyalardaki iptal-güvensiz okuma
  düzeldi (DESIGN §5.7).
- [x] **`protoc-bin-vendored` bağlı değil** — **KAPANDI** (protokol
  sertleştirme turu, `5e21a09`). `gsb-protocol` VE `gsb-game` build
  script'leri gömülü ikiliyi `Config::protoc_executable` ile veriyor;
  açık yol olduğu için `PROTOC`/`PATH` aramasının önüne geçiyor. CI iki
  işte de `protobuf-compiler` kurmuyor — bağlamanın bozulması CI'ı
  düşürür, davranış kilidi bu.
- [x] **`loadgen/churn.rs` log string'inde gömülü boşluk blokları** —
  **KAPANDI** (teknik borç turu, `4465c47`): literal `\` devamıyla
  yeniden yazıldı, gömülü 38'er boşluk gitti. Bu satır kapandıktan sonra
  da açık duruyordu; rUDP doğruluk turunun doküman taramasında yakalandı
  (kodda doğrulandı: `churn.rs`'te 38 boşlukluk koşu kalmadı).

## Protokol sözleşmesi (protokol sertleştirme turu sonrası)

Kapanmış maddeler — yeniden açılmadan önce `docs/DESIGN.md` §5.2-5.5
okunmalı:

- [x] **RPC zarfının iki yarısı da base'de** (`3bab835`, DESIGN §5.2).
  `RpcResponse` `base.proto`'ya taşındı, `game.proto` import ediyor;
  `extern_path` ile tip ikinci kez üretilmiyor. Mesaj sahipliği kuralı
  §5.2'de: sözleşmeyi core belirliyorsa mesaj base'dedir.
- [x] **`reserved` disiplini** (`00de2b3`, DESIGN §5.3). `base.proto`'dan
  hiç alan kaldırılmamış (denetlendi, not edildi); `game.proto`'daki tek
  kaldırma (`EntityState.version = 4`) `EntityRecord`'da rezerve;
  emekli opcode'lar `gsb_game::op::RETIRED` + test.
- [x] **ERROR kodları üretilen enum** (`f878ad7`, DESIGN §5.4).
  `gsb.base.ErrorCode`; iki Rust eşlemesi de tüketici match, yeni varyant
  derlemeyi kırar. İleri uyumluluk kuralı (bilinmeyen kod → OTHER, 0 asla
  gönderilmez) belgelendi ve testlendi.
- [x] **Protokol sürümü** (`9fe6beb`, DESIGN §5.5). `Auth.
  protocol_version` + `PROTOCOL_VERSION` + ERROR 13. Politika TAM
  EŞİTLİK; `0` = sürümsüz, kabul.
  **Tetikleyici (açık bırakılan tek parça):** ikinci bir protokol sürümü
  gerçekten yayınlandığında min/max ARALIK politikası (ya da özellik
  pazarlığı) tartışılacak. Bugün tek bir doğru değer var, aralık
  tetikleyicisiz esneklik olurdu. `PROTOCOL_VERSION` yalnız ESKİ bir eşi
  YANLIŞ AYRIŞTIRACAK bir değişiklikte artar (alan wire type'ı, opcode
  geri dönüşümü, çerçeveleme); toplamalı değişiklikte ARTMAZ.

## Bilinçli olarak yapılmayanlar (referans)

- Cross-server / cross-region, kalıcılık, yük dengeleyici — v1 kapsam dışı.
- bevy `Event`/observer sistemi hot path'te kullanılmıyor — bilerek
  (snapshot kararı wire içeriğiyle; ayrı bir versiyon bileşeni yok —
  eski `EntityVersion`/`bump()` denetim turunda kaldırıldı; gerekirse
  delta yayınında (P2) yeniden getirilir; `DESIGN.md` §7).
