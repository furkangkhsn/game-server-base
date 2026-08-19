# Ne kaldı? — gsb Geliştirme Listesi

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
(500/1000/2000 + hücre boyutu taramasıyla ölçüldü, aşağıda).
44/44 test yeşil (+1 var olan `#[ignore]`'li gsb-lint doctest).
Aşağıdakiler **ölçülmemiş performans** (10k+ ölçek henüz ölçülmedi;
100/500/1000/2000 turu aşağıda), **robustluk** ve **güvenlik**
başlıklarındaki kalan işler.

## Kapatılanlar (yayınlanabilirlik turu)

- [x] **Yayınlanabilirlik ön koşulu yorumdaydı — yapısal hale getirildi**
  — `0bf5b79`, snapshot sorgusunu `(Entity, &Position)`'tan
  `(&WireId, &Position)`'a çevirdi ve `WireId` yalnız `on_join` içinde
  atanıyordu: `Position` taşıyan ama `on_join`'den geçmemiş bir entity
  (mermi/NPC/tuzak gibi oyuncuya bağlı olmayan ilk entity) hiçbir
  istemciye **hiç** görünmez olacaktı — hata yok, uyarı yok, sadece
  yok. Bu, aynı dosyada bu sınıfın üçüncü örneğiydi (`bump()`
  disiplini — F1; `GroupState::members` — F3) ve ikisinde de kural
  yalnız yorumda yaşıyordu; bu yüzden "belgele" bu turun çıtası
  değil. **Seçilen çözüm: (a) entity bir sonraki snapshot'ta görünür** —
  broadcast geçişi, `Position` taşıyıp `WireId` taşımayan entity'lere
  odanın **tek monoton sayacından** taze serial damgalar (iki aşamalı:
  yetim sorgusu `query_filtered::<(Entity, &Position), Without<WireId>>`
  ile toplar, damga yazılır, tam sorgu damgalardan SONRA çalışır — her
  entity tam olarak bir kez toplanır; ilk denemede yetim hem damga
  hem tam sorguda toplanınca double-count yakalandı ve düzeltildi).
  **Neden (a), neden (b) "odanın tespit edip bildirmesi" değil:**
  (i) `0bf5b79` öncesi sözleşme "Position ⇒ yayın" idi; (a) bu
  sözleşmeyi yeni kimlik uzayıyla geri koyar, (b) daraltılmış kümeyi
  koruyup yalnız bir log satırı eklerdi — spec'in "sessizce kaybolmak
  kabul değil" çıtasını (b) de karşılar ama motifi (mermi eklendiğinde
  görünmez entity) yaşatırdı. (ii) (b) "spawn'da WireId damgala"
  **disiplinini** yaşatır — bu turun "kural yalnız yorumda yaşamasın"
  çıtasının tam karşısı; gelecekteki mermi geliştiricisi (b)'de
  görünmez entity + uyarı logu alır, (a)'da çalışır entity.
  (iii) Oda-seviye tanı (F4 deseni) yalnız oda **yetkiyi
  kullanamadığında** doğrudur (opak payload, ayırt edilemez gözlem
  serisi); oyun mantığı kendi dünyası üzerinde tam yetkilidir — rapor
  etmek değil, düzeltmek gerekir. **Değerlendirilip elenen
  alternatifler:** (b) tespit + `warn!`, görünmezlik kalsın —
  yukarıdaki üç gerekçeyle; (c) yetim entity'de `expect`/panic — normal
  bir gelecek oyun deseni (mermi) sunucu çöküşüne çevrilirdi ve
  "hata yok, uyarı yok"u "server crash"e çevirmek spek'in işaret
  ettiği yönün tersidir; (d) sorguyu `(Entity, &Position)`'a geri
  alıp bevy bits'i telde taşımak — kimlik turunda **ölçülerek**
  elenmişti (5 B varint + slot yeniden kullanımında kimlik
  çarpışması → kimlik değişmezi kırılır); (e) damgayı `update()`
  (SYSTEMS) fazına koymak — çalışır (tick-içi spawn hep snapshot
  öncesi damgalanır) ama tek boğaz noktası `snapshot()`'ta değil
  fazda kalır: ingest/system/test hangi fazda spawn ederse etsin
  snapshot yakalar, (e) yalnız "bu tick'te update() çalıştı" yolunu
  garanti eder — boğaz noktasını asıl tüketiciye (snapshot) koyduk.
  **Sınıfı kapatır mı: KAPATIR** — "yayınlanabilirlik ön koşulu
  yalnız yorumda yaşıyor" sınıfının demo tarafındaki tüm örnekleri
  bu değişiklikle kod tarafına taşındı: yayın kümesi artık sorgu +
  damga ile tanımlı, yorum değil. Kalan tek "yorumda yaşayan" üye
  grup başına defter şartı (F4; yapısal olarak kapatılamaz —
  aşağıda MADDE 3). Kimlik değişmezi korunur: tek sayaç, monoton,
  asla yeniden verilmez — `wire_identity_survives_ecs_slot_reuse`
  hâlen geçerli ve yeni atama noktası aynı sayaçtan beslenir.
  **Regresyon (seçimi kilitleyen test):**
  `entity_spawned_outside_on_join_is_broadcast_with_fresh_wire_id` —
  entity `on_join` **olmadan** doğrudan world'e spawn edilir;
  snapshot'ta görünür (taze serial 3), damga idempotent (içerik
  değişmezse sessiz + kimlik sabit), hareket sonrası **aynı** kimlik
  yeni konumda. **Tel boyutu etkisi: ölçüldü, SIFIR** (probe; gerçek
  `RoomActor` + `DemoRoom` + kanallar; probe commit edilmedi):
  N=1: **10 B** (8,00 B/kayıt) · N=100: **796 B** (7,94) · N=800:
  **7017 B** (8,77) · 1400 B eşiği **N=171**'de aşıldı (1402 B; N=170
  = 1400 B → eşik aşılmaz). Öncesi (`0bf5b79` worktree, aynı probe)
  ile sonrası **bire bir aynı** — kararlı durumda yetim sorgusu boş
  (O(0)) ve payload kayıt-kayıt aynı. Önceki turun kayıtlı "1403 B"
  değeri bu probe'un "1402 B" değerinden 1 bayt farklı: bu probe tüm
  join'leri tek tick'te işler (`sequence=1` → 1 B varint); eski probe
  snapshot'ı `sequence ≥ 128`'de aldığında 2 B varint olur — eşik
  N'si (171) değişmez. Sayılar bağımsız modelle de (spawn formülü +
  proto3 kodlaması) bire bir doğrulandı.
- [x] **`Owner` ölü durumu kaldırıldı** — `Owner(ConnectionId)` her
  `on_join`'de yazılıyor ama hiçbir yerde okunmuyordu — birebir
  `GroupState::members` (F3) kategorisi: yazılıp okunmayan durum.
  "Gelecekteki mermi/NPC sistemleri shooter'a ihtiyaç duyarsa kalsın"
  alternatifi **reddedildi**: (i) spekülatif — kod tabanının kendi
  emelesi (F1/F3/Bulgu 2): okuyucusuz yazım hot path'te bedava değil,
  ölü durum silinir ve **ilk gerçek okuyucuyla birlikte** geri gelir;
  (ii) `conn_entity` haritasının (conn → entity) aynısını ters
  yönde ikinci bir kaynakta tutuyordu — iki sahiplik kaydı, biri
  ölü; (iii) 8 B/entity + spawn'da yazım bedeli. Bir okuyucusu
  YOK; gerekirse gerekçesiyle geri getirilir.
- [x] **MADDE 3 taraması — aynı sınıftan başka kalıntı** — son iki
  turun değişikliklerinin (wire kimliği, wire içeriği detector'ı)
  ardında tüm workspace "yalnızca yorumda yaşayan ön koşul" / "yazılıp
  okunmayan durum" diye tarandı (alan-bazında read/write sayımı +
  doküman taraması):
  1. **`TickCtx.room` yazılıp okunmuyor** — oda actor'ü doldurur;
     hiçbir mantık okumaz. **Korundu**, gerekçe: `RoomLogic`'in
     halka API'si (pub alan) — Owner'dan farkı: oyun-özel component
     değil; oyun mantığının kendi oda kimliğini öğrenmesinin **tek**
     yolu (ikinci kaynak yok, duplikasyon yok) ve gerçek
     çok-odalı mantıkta doğal okuyucusu var. Bugün okunmaması
     demo sadeliği; ölü durum değil.
  2. **`CoreError::RoomFull` hiç kurulmuyor** — pub varyant, P1
     `max_players` maddesinin taşıyıcısı (rezerve API yüzeyi —
     `PRIVATE` op'undaki "reserved, demo kullanmaz" deseniyle aynı
     sınıf). Kod değişiklığı yok; ROADMAP P1 maddesindeki "yeniden
     eklenecek" ifadesi güncellendi (zaten mevcut — maddenin ifadesi
     eskiydi).
  3. **Stale "4-phase tick" doküman kalıntıları (2 dosya) düzeltildi**
     — denetim turu `gsb-core` manifest tanımını 5 faza çevirmiş ama
     `gsb-core/src/lib.rs` ve `gsb-server/src/lib.rs` modül
     yorumlarındaki "four-phase/4-phase" kalmıştı; "mevcut olmayan
     mekanizmayı anlatan doküman" sınıfından iki kalıntı, aynı turda
     kapatıldı.
  4. **Grup başına defter şartı — sınıfın bilinen ve yapısal olarak
     kapatılamaz tek üyesi** (F4; 2aad7ea turunda odanın tick-başına
     gözlemi tek tek sayılarak yeniden doğrulandı): oda, aç kalan
     grup ile içeriği gerçekten donan meşru grubu ayırt edemez →
     koruma sözleşme metnidir (DESIGN §4 Tanı). Yeni değil; kaydı
     burada tutulur, bu turda değişmedi.
  5. **"Wire kimliği yalnız oda sayacından atanır" bir konvansiyon** —
     modül dokümanı + proto yorumunda yaşar. Bugünkü **tüm** atama
     noktaları (`on_join` + yeni broadcast damgası) tek sayaçtan
     geçer (grep ile doğrulandı); slot yeniden kullanımı tehdidi
     `wire_identity_survives_ecs_slot_reuse` ile kilitli. Yapısal
     olarak kapatılamaz: component tipi allokatörü sahiplenemez,
     crate-içi manuel `WireId(42)` ataması (test/bozuk mantık)
     alan görünürlüğüyle de önlenemez — bu, tesadüfi "eksik ön
     koşul" değil, **bilinçli kötü kullanım** sınıfı (sınıfın
     kapsamı dışında). Not: bu turun damgası ikinci atama noktasını
     ekledi — her ikisi de aynı sayaç, değişmez bozulmadı.
  6. `TICK-ARCHITECTURE.md`'deki "4 faz" tablosu: dosya başlığı
     gereği **tasarım tartışmasının arşivi** — tarihsel anın doğru
     betimi, dokunulmadı.

## Kapatılanlar (wire kimliği turu)

- [x] **`EntityRecord.entity` kompakt kimlik: tam bevy bits → oda-yerel
  monoton serial** — F6 turunda ölçülen gerçek durum: kayıt tipik 10 B
  ve bunun 5 B'si yalnızca entity kimliği (bevy 0.19'da `to_bits()`'in
  alt 32 biti `0xFFFFFFFF - index` olduğundan varint pratikte her zaman
  5 bayt; eski nottaki "bit tersi" ifadesi ölçümde yanlış çıktı, aslı
  bit komplementi). 1400 B eşiği ~117 entity'de aşıyordu. Çözüm: oda,
  spawn başına sıradaki wire kimliğini (1'den monoton artan) entity'ye
  `WireId` componenti olarak stamp'lar ve **oda ömrü boyunca bir değeri
  asla yeniden vermez**; aynı değer `JOIN_ROOM_RESULT`'a gider (iki yol
  tek uzayda; `gsb-core`'daki opak `EntityId`'a dokunulmadı — yalnız
  doküman düzeltildi). Kimlik değişmezi nasıl korunuyor: istemcinin
  dünya görüşü son kabul ettiği snapshot'tır (delta/geçmiş/out-of-band
  remap yok); iki farklı entity oda ömrü boyunca aynı tel kimliğini
  paylaşılamayacağından, kimlik hem eski hem yeni snapshot'taysa aynı
  entity (hareket), yalnız yeni snapshot'taysa yeni entity — kayıp
  snapshot olsa bile. Değerlendirilen ve elenen alternatifler:
  (a) yalnız bevy *index* (1-2 bayt) — slot yeniden kullanıldığında
  kimlik çarpışır; bevy 0.19'da yeniden kullanım her 129 free'den sonra
  gerçekleşiyor (ölçüldü: yerel free buffer 128) ve kayıp snapshot
  durumunda istemci yeni entity'yi "eski entity teleport oldu" diye
  okur → değişmez kırılır; ayrıca protokol, bevy allocator'ının iç
  davranışına sessiz bağımlılık kazanırdı. (b) tam bits + başka varint
  kodlaması — alt 32 bit ≈ 2^32 olduğundan 5 baytun altına inemez.
  Ölçüm (gerçek `RoomActor` + `DemoRoom` + kanallar; önceki turun
  yönteminin birebir tekrarı, ham çıktı commit raporunda):
  1 entity = 14 → 10 B; 100 = 1196 → 796 B; 800 = 9544 → 7017 B
  (marjinal ~11,93 → ~8,77 B/kayıt); entity varint histogramı N=1/100:
  5B×N → 1B×N; N=800: 5B×800 → 1B×127 + 2B×673 (serial 128'den sonra
  2 bayta geçer); 1400 B eşiği N=118 (1410 B) → **N=171 (1403 B)**.
  Serial'ın büyüme karakteri: oda spawn sayacı < 128 → 1 B, < 16384 →
  2 B, < 2M → 3 B (uzun ömürlü, çoklu churn'li odalarda tipik kayıt
  9 B). Regresyon: `wire_identity_survives_ecs_slot_reuse` — 129
  join/leave döngüsüyle bevy slot yeniden kullanımını zorlar (geri
  dönen index'in eski entity'nin index'i olduğunu ve generation'ın
  yükseldiğini assert ederek testi boş olmaktan korur) ve geri dönen
  slotun taze wire kimliği taşıdığını doğrular: kayıp snapshot'ları
  olan istemci yeni entity'yi "yeni entity" olarak okur. Mevcut
  e2e/late_join testleri semantiğiyle aynen korundu (join result
  kimliği snapshot'ta aynı uzayda bulunuyor); `snapshot_emits_on_
  plain_position_write` uyarlandı: `on_join` artık serial döndürdüğü
  için bevy handle `conn_entity` üzerinden okunuyor (amaç/assertler
  aynı).

## Kapatılanlar (2aad7ea denetim turu)

- [x] **F5: `keepalive_hz > tick_hz` sessizce devre dışı kalıyordu** —
  `((tick_hz / keepalive_hz).round() as u64).max(1)` oranı < 1 olunca
  keepalive adımına clamp'leniyor, "değişiklik yoksa sessizlik"
  kazancı tamamen kapanıyordu (tick 30 + keepalive 60 → her adım
  yeniden gönderim); yapılandırmada ilişki kontrol edilmiyordu,
  çalışma anında log/metrik yoktu. Düzeltme: (1) registry `CreateRoom`
  içinde `keepalive_hz > tick_hz` (ve `> 0`) yeni
  `CoreError::KeepaliveRate` ile reddediyor — mevcut `TickRate`
  reddinin aynısı (hız ilişkisi
  yapılandırma invariant'i; sessiz degrade edilmiş oda başlamasın);
  (2) doğrudan inşayı (lib kullanımı) koruyan ikinci katman:
  `RoomActor::new` aynı ilişkide **bir kez** `warn!` veriyor (iki
  hızı da adlandırarak) ve clamp davranışını belgeliyor;
  (3) `config.example.toml` + `RoomConfig` dokümanına `≤ tick_hz`
  şartı. Testler: `create_room_rejects_keepalive_above_tick_rate`
  (120>60 reddedilir; == tick, < tick ve 0 kabul) +
  `keepalive_above_tick_warns_at_construction_and_clamps_to_every_step`
  (doğrudan inşa: uyarı tam bir kez; clamp'li davranış — join
  yayınından sonra HER adım yeniden gönderim, 6/6 batch; meşru
  oranlar == tick ve 1 Hz uyarı vermez). Not: metrik altyapısı yok
  (P0 "Temel metrik" hâlâ açık) — mevcut sinyal log.
- [x] **F6: "21→16 B wire iddiası" ölçülerek düzeltildi** — iddia
  ROADMAP snapshot maddesinde duruyordu (`3f25874` denetim turunun F6
  bulgusu sayıların hatalı olduğunu saptamıştı; `0888441`'in
  sfixed32→sint32 geçişinden sonra iddia tamamen geçersizdi). Gerçek ölçüm (probe; gerçek `RoomActor` +
  `DemoRoom` + kanallar, ham çıktı commit raporunda): yeni `EntityRecord`
  tipik 10 B (3 tag + 5 B entity varint + 2 × 1 B zigzag koordinat;
  koordinat 0 → proto3 default → 8 B kayıt) + snapshot'ta kayıt başına
  2 B çerçeve; toplam: 1 entity = 14 B, 100 = 1196 B, 800 = 9544 B
  (marjinal ~11,93 B/kayıt). Eski `sfixed32` + `version` kaydı aynı
  prost ile 18 B — yani asıl geçiş ~18 → ~12 B, iddia edilen 21→16
  değildi. DESIGN §8'e ölçülmüş sayılar + bevy 0.19 entity-bit notu
  (index bit tersi → varint pratikte her zaman 5 B) eklendi; 1400 B
  eşik ~117 entity'de aşılır. ROADMAP snapshot maddesi düzeltildi.
- [x] **F4 hükmü yeniden incelendi: "oda bu modu tespit edemez"
  doğrulandı** — önceki turda F4 tanısının yalnızca "ömrü boyunca hiç
  yayınlamamış" grubu yakaladığı, paylaşımlı defterli kaybeden
  grubun kendi join tick'inde bir kez yayın yaptığı için tanının hiç
  tetiklenmediği ölçülmüş (1/26 modu, 0 uyarı) ve mod "yapısal olarak
  tespit edilemez" denilmişti. Bu turda hükmün gerekçesi somutlaştırıldı
  ve DESIGN §4 Tanı maddesine taşındı: odanın tick başına gözlemi
  (grup tablosu/`GroupState`, üyelik, önceki tick durumu, `snapshot()`
  dönüşü) tek tek sayıldı; aç kalan grup ile son yayınından sonra
  içeriği gerçekten donan meşru grup (AOI'de tek hareketli entity
  alanı terk eder; demo'da oyuncu hedefine ulaşır) bu gözlem serisine
  bire bir aynı düşüyor → ayırt edici sinyal yok; payload core'a opak
  (core/oyun sınırı), `sequence` bu modda ilerlemiyor, ack yok,
  keepalive `last`'i klonluyor. Hüküm doğru → kod değişikliği
  yapılmadı; doküman notu bu gerekçeyle netleştirildi.

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
  meşru sessizliği ayırt edemez, koruma sözleşme metnidir. (Hüküm,
  2aad7ea denetim turunda odanın tick başına gözlemi tek tek sayılarak
  yeniden sınandı ve doğrulandı: aç kalan grup ile içeriği gerçekten
  donan meşru grup aynı gözlem serisini üretir — bkz. "Kapatılanlar
  (2aad7ea denetim turu)" F4 maddesi ve DESIGN §4 Tanı.) Yan
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
- F5 ve F6 (bu turun bulgusu) 2aad7ea denetim turunda kapatıldı
  (yukarıda, "Kapatılanlar (2aad7ea denetim turu)").

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
  kayıtları + monoton `sequence`); kayıt başına versiyon alanı düştü —
  sıralamayı `sequence` üstleniyor. (Wire boyutu için ölçülmüş sayılar:
  yukarıda "2aad7ea denetim turu" F6 maddesi + DESIGN §8; eski "21→16 B"
  iddiası ölçümden geçmedi: aslı ~18 B → ~12 B.) Değişiklik yoksa yayın
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

## Kapatılanlar (metrik + yük turu)

- [x] **`WireId` mint tarafı kapandı (tip düzeyinde)** — yayınlanabilirlik
  turunda "yapısal olarak kapatılamaz" denilen son parça: field private
  (`WireId(u64)` artık hiçbir crate'ten, crate içinde de `components`
  modülü dışından `WireId(42)` diye inşa edilemez) + `Default` derive'ı
  kaldırıldı (`WireId::default()` = 0 = "atanmamış" kimliği üretilemez).
  Tek inşaat yolu `WireId::new(u64)` (`pub(crate)`, `const`); okuma tarafı
  açık kalmalıydı (`pub const fn get(self)`) — kimlik okumak meşru,
  üretmek değil. Tek minting noktası `DemoRoom::next_serial()`
  (sayaç `next_wire_id`): iki çağrı noktası (`on_join` + broadcast
  yetim-damgası) bu tek noktanın üzerinden geçer. Kapanan hata sınıfı:
  "kimlik üretimi sözleşmede yaşıyordu" → artık üretimin tek yolu tipin
  kendi kapısı ve pratikte tek sayaç. Elenen alternatifler:
  (a) `pub fn new`'i herkese açık tutmak — her crate mint edebilir,
  çakışma alanı kapanmıyor (spec: tek nokta isteniyordu).
  (b) `Arc<AtomicU64>` paylaşımlı sayaç — paylaşımlı durum = DESIGN §2
  ihlali; zaten sayacın sahibi oda actor'ünün yerel durumuydu (tek sahip
  mevcut). (c) Sealed trait/free function seremonisi — aynı kapanımı
  daha fazla yüzeyle verirdi.

- [x] **Metrik altyapısı (P0 "Temel metrik")** — Sayaçlar **ait olduğu
  aktörün yerel durumunda** ve dışarı **kanalla** taşınır
  (`gsb_core::metrics`): oda actor'ü her adım sonunda (tick gövdesinin
  son satırı, senkron) `MetricsEvent::Room(RoomSample)` gönderir;
  registry olay başına (open/close/join/leave/room); bağlantı actor'ü
  ≤1/sn + kapanışta son gönderim (in/out byte/frame delta). Kanal
  `mpsc::unbounded` — **`send` senkron** (`Result` döner, `Future`
  değil): oda actor'ünün tick döngüsüne **await eklenmedi**; tek await
  hâlâ `tick_rx.recv()`'tir (spec'in sert şartı — bounded kanal
  `send`'i Future olduğundan elendi). Tüketiciler:
  `MetricsCollector` görevi — tek await'i ticker'ın **aynı**
  broadcast aboneliği (odalarla aynı saat kaynağı; tek-await
  disiplininin aynası); her tick'te `try_recv` ile boşaltır, rapor
  süresi (varsayılan 1 s) dolunca `MetricReport` üretir, ticker
  kapalıysa son raporu basıp temiz çıkar (shutdown kaskadının doğal
  parçası — DESIGN §9). Sink'ler: `Log` (`RUST_LOG=info` →
  `gsb-metric scope=.. key=value` satırları) ve `Channel`
  (→ `MetricReport` — `start_server_metrics` ve testler bu yoldan).
  Ölçülenler (spec'in istediği sorular): konfigure **vs gerçek** tick
  hızı (oda `hz` Δadım/rapor + registry oda sayısı), **adım süresi
  dağılımı** (min/mean/max + 7 kutulu histogram µs), **gecikme**
  (`late_*` — tick'e ulaşma gecikmesi) ve `lagged_*` (broadcast
  tamponu aşımı), **atılan batch** (`dropped`), **keepalive yeniden
  gönderimi** (`keepalive_resends`), **grup sayısı/üye**
  (`groups`/`members`/`max_group`), **snapshot yükü**
  (`snapshots`, `snap_bytes_s`, `snap_bytes_max`, `shipped_*` —
  kodlanan vs dağıtılan), **bağlantı sayısı/akışı**
  (registry `conns`/`opens`/`closes`/`joins`/`leaves`), **byte
  in/out** (conn actor delta + net toplam). Kapanan hata sınıfı:
  "log satırı dışında ölçüm yoktu" — her sunucu süreci artık
  kendisinden (log ya da kanal) okunabilir. Kanıt testi
  (`metrics::tests::room_counters_flow_to_collector`): gerçek
  `RoomActor` + gerçek collector — oda actor'ünün **yerel** sayacı
  (özellikle `dropped > 0`: tüketilmeyen çıkış kanalı) actor'ün
  *dışından*, kanal üzerinden üretilen raporda doğrulanır; birikim +
  hız penceresi ayrı unit testte. Elenen alternatifler:
  (a) `AtomicU64` küresel kayıt — paylaşımlı mutasyon = §2 ihlali
  ("her değer kanallarla taşınır, paylaşım yoktur"); toplayıcının
  çıktısı mesaj akışının saf fonksiyonu olmaktan çıkar ve hangi
  aktörün hangi sayacı tuttuğu görünmez olurdu.
  (b) Bounded kanal + geri-baskı — bounded `send` Future → tick
  gövdesinde await → "oda tek await'i `tick_rx.recv()`'tir" şartı
  kırılır (spec bunu açıkça yasaklıyor). Örnekler sabit boyutlu ve
  oda başına adım başına en fazla bir tane olduğundan unbounded'da
  geri-baskı fiilen sorun değil; en kötü hal = örnek kaybı, tick asla
  durmaz. (c) Collector'da `tokio::select!` (tick + kanal) —
  gsb-lint `select!`'i yasaklar; ticker aboneliği saat olarak yeterli
  (rapor ≥1 s, tick 30/s). (d) Prometheus tarzı gauge kütüphanesi —
  v1 kapsamı dışında; `MetricReport` zaten dış tüketim için yapısal
  (exporter P2 adayı).

- [x] **Load test harness'i (P0) + ilk uçtan uca sayılar** —
  `gsb-loadgen` binary'si (`crates/gsb-server`, **kalıcı araç olarak
  commit edildi** — spec'in "örnek binary" niyetini `[[bin]]` olarak
  yerine getirir; gerekçe: duman testi `CARGO_BIN_EXE_gsb-loadgen` ile
  spawn'layabilsin ve gerçek `RESULT` satırı üretilsin). Modlar:
  **in-process** (gerçek sunucu `start_server_metrics` ile ephemeral
  portta + N gerçek TCP istemcisi aynı runtime'ta — sunucu tarafı
  metrikler kanaldan, stdout parse'ı **yok**) ve **external**
  (`--addr` ile yalnız istemci tarafı). Yapılandırma: N, `--duration`,
  `--move-ms`, `--room`, `--stagger-ms` (istek i, i×ms gecikmeyle
  bağlanır — 0 = hepsi birden), `--addr`. Duman testi
  (`tests/loadgen_smoke.rs`): binary spawn edilir (3 istemci × 3 s),
  `RESULT` satırı parse edilir — 3/3 connect+join+leave, istemci VE
  **sunucu tarafı** tick hızı 20–40 Hz bandında, snapshot akışı ve
  sunucu byte sayaçları > 0 (suite 3.2 sn; ağırlıklı koşular bilinçli
  olarak suite dışında: spec "normal suite'ü yavaşlatmasın, flaky
  yapmasın" — 10 sn × ölçek başına in-suite koşul bu şartı bozardı).
  Elenen alternatifler: (a) suite içinde N=1000 test — yavaş + flaky
  (CI yükünde); (b) sunucu stdout'unu parse eden dış araç — sunucu
  tarafı metrikleri yapısal olarak yakalayamazdı (hız/histogram),
  log formatına sessiz bağımlılık doğar; kanal sink'i zaten üretim
  sunucunun kullandığı yol. (c) Hazır yük kütüphanesi (k6/locust) —
  gsb frame protokolünü (AUTH/JOIN/MOVE/SNAPSHOT) konuşan istemci
  yine elle yazılacaktı; tek binary in-tree duman testini bir satır
  uzakta tutuyor.
  **Yöntem (dürüstlük cümlesi):** istemciler ve sunucu **aynı
  makinede** (in-process modda aynı runtime'ta; 32 çekirdek,
  release profile, load avg ≈2.5–3.3, somaxconn 4096) çalıştığından
  ölçülen sunucu kapasitesi **alt sınırdır** — istemci işi (decode,
  read loop, tekte 227 MB/s alma) sunucunun CPU'suyla yarışır; ayrı
  yük makinesinde sayılar yukarı çıkar.
  **Ham sayılar (10 s pencere, 30 Hz oda, tek oda, 150 ms MOVE_TO;
  release, 32 çekirdek):**

  | Metrik | N=100 (burst) | N=500 (burst) | N=1000 (burst) | N=1000 (stagger 2ms) |
  |---|---|---|---|---|
  | connected / joined / left | 100/100/100 | 500/500/500 | 1000/**748**/748 | 1000/1000/1000 |
  | hatalı istemci | 0 | 0 | 0 | 0 |
  | connect p50 / p99 | 14ms / 14ms | 1012ms / 1076ms | 1006ms / 1068ms | 0ms / 5ms |
  | snapshot/istemci (p50) | 292 | 268 | 261 | 270 |
  | istemci tick hızı (snapshot `sequence`, median) | 30.00 Hz | 30.00 Hz | 30.00 Hz | 30.00 Hz |
  | **sunucu** tick hızı (raporlar median) | 30.00 Hz | 30.00 Hz | 30.00 Hz | 29.99 Hz |
  | oda adımı (10 s) | 304 | 304 | 304 | 304 |
  | adım süresi p50 / max | 75µs / 240µs | 375µs / 931µs | 3000µs / 4305µs | 3000µs / 2822µs |
  | adım histogramı (µs; [0,50) [50,100) [100,250) [250,500) [500,1k) [1k,5k) [5k,∞)) | [115,147,42,0,0,0,0] | [9,6,42,130,117,0,0] | [8,3,21,9,108,155,0] | [3,3,12,11,18,257,0] |
  | tick gecikmesi max (`late`) | 27µs | 23µs | 37µs | 20µs |
  | **atılan batch** | 0 | 0 | 0 | 0 |
  | lagged event/tick | 0/0 | 0/0 | 0/0 | 0/0 |
  | sunucu çıkış (fan-out, net) | 2.30 MB/s | 58.06 MB/s | 128.84 MB/s | 227.11 MB/s |
  | tepe snapshot payload | 798 B | 4334 B | 6543 B | 8807 B |
  | tepe bağlantı | 100 | 500 | 748 | 1000 |

  **İlk doyma metriği (ölçüm, tahmin değil):** 100 ve 500'de hiçbir
  metrik doyuyor değil — tick hızı iki tarafta da 30.00, drop 0,
  `late` ≈20-27µs, adım p50 bütçenin (33.3ms @30Hz) %0.2-1.1'i.
  N ile **büyüyen tek metrik adım süresi**: 75µs (100) → 375µs (500)
  → 3000µs (1000) p50 — 1000 üyede bütçenin ~%9'u (p99≈3ms, max
  2.8–4.3ms); histogramın ağırlığı [1k,5k) kutusunda. Bu metrik
  fan-out'un maliyeti (snapshot kodlama O(entity) + bağlantı başına
  Arc klonu + `try_send`): N büyüdükçe ilk doyaan şey adım bütçesi
  olacak — doğrusal dışa vurumla (3ms@1000 → 33ms) tek odada
  ~10k üye bütçeyi doldurur; bu tam olarak P2'nin (AOI, oda
  bölme) hedeflediği duvar. Drop 0'ın nedeni: 256 derinlikli
  `conn_out` kanalı + hızlı istemci fan-out'u emer; 227 MB/s
  loopback çıkışı da sorun yaratmıyor.
  **Burst bağlantı bulgusu (spec dışı, dürüst raporlama):**
  N≥~150 **eşzamanlı** connect (loopback) 10 s pencerede joined<N
  yapıyor (koşu bazında 748–956/1000; her koşuda ~1 s'lik dalgalar).
  Kök neden minimal probe ile izole edildi (gsb kodu **yok**, sıfır
  işli ham tokio accept loop): tokio/mio'nun edge-triggered
  (EPOLLET) readiness'i burst'te accept görevini durum-değişimi
  kenarı başına uyandırıyor — kuyruk dolu kalırken yeni kenar
  gelmiyor, istemci ~1 s sonra SYN yinelerek uyanıyor. Aynı
  makinede native (std) accept loop 1000 bağlantıyı **122ms**'de
  kabul ediyor; kernel kuyruğu suçlu değil (backlog 1024, kimse
  accept etmese bile 500 el sıkışma <200ms'de tamamlanıyor); ve
  **stagger'lı** istemcilerle (1ms aralık → 1000/s) hem probe hem
  gsb accept loop sıfır dalga ile yetişiyor. Sonuç: gerçekçi
  kademeli join'de bağlantı kurma sorunsuz (stagger koşusu:
  connect p50=0ms, 1000/1000 join); burst sayıları loopback
  burst'ının tokio accept yolundaki bir artefaktıdır — v1 bulgusu
  olarak raporlanır, actor modeli kapsamında düzeltme yok.

## Kapatılanlar (metrik düzeltme + AOI turu)

Bu tur iki işi kapatır: (A) metrik altyapısının üç ölçüm hatası (spec),
(B) AOI — tek odada bağlantı başına bant genişliğini O(entity) → O(görünürlük
kümesi) yapar; ana soru "**`gsb-game` içinde `gsb-core`'e dokunmadan yapılabilir
mi?**" → **evet** (aşağıda).

- [x] **A1 — Adım histogramı tick bütçesine göre (oran)** — Eski
  `HIST_EDGES_US=[50,100,250,500,1000,5000]` (mutlak µs) üst kutusu
  `[5000,∞)`'dü; 30 Hz bütçesi 33.3 ms olduğundan 5.1 ms'lik ve 40 ms'lik
  adım **aynı** kutuya düşüyordu ve "bütçe aşıldı mı?" histogramdan
  okunamıyordu. Yeni: kenarlar tick bütçesinin (bir periyot, µs) **oranları**
  — log-2 merdiveni `1/128× … 1× … 32×` (`HIST_EDGES`). `(1,1)` kenarı **tam
  tick bütçesidir**; bin `HIST_OVERFLOW_BIN` (=8) ve üzeri **bütçe aşımı**
  (oda hızını tutamıyorsa). Aşağıya 1/128×'e inen basamaklar sağlıklı oda
  (adım ≪ bütçe) için: 30 Hz'de (bütçe 33 333 µs) alt kutular
  ~261/521/1042 µs'e oturur — gerçek adım sürelerinin (≈500 µs) yeri; p50
  artık anlamlı (eski hâl her şeyi tek devasa `[0, 0.5×bütçe)` kutusuna
  yığıp p50≈8.3 ms sahte değeri veriyordu — ölçüldü, düzeltme sonrası
  500'de p50≈391 µs). Neden oran (mutlak µs değil): mimari 15/30/60 Hz oda
  destekliyor (DESIGN §10); oran bütçeyi **her hızda** bir kutu sınırı yapar —
  spec'in kriteri tam "bütçe aşımı histogramdan okunabilir mi". Kanıt:
  `hist_index_binning` (30 Hz bütçede 5.1 ms ≠ 40 ms; 40 ms'ı
  `HIST_OVERFLOW_BIN`). Kapanan hata sınıfı: "üst kutu bütçeyi göremiyor".
  Elenen: (a) mutlak µs + bütçeyi ayrı tutmak — kriteri karşılar ama iki
  kaynak (kenar+bütçe) her hızda ayrı ayar ister; (b) 1/2×'ten başlayan kısa
  merdiven — kriteri karşılar ama sağlıklı odanın dağılımı tek kutuya yığılır
  (p50 bozulur, ölçülen).

- [x] **A2 — Örnek gönderim temposu rapor temposuna bağlandı + oran örnek
  aralığı üzerinden** — Oda actor'ü eskiden **her tick'te** (30/sn) tam
  `RoomSample` gönderiyor; toplayıcı yalnız `acc.latest`'i tutup 1/s rapor
  üretiyordu → 29/30 örnek atılıyordu. `RoomConfig.metrics_cadence_hz`
  (vars. 1.0) ile oda her `round(tick_hz/cadence)` adımda (vars. 30) bir kez
  gönderiyor; sayaçlar kümülatif olduğu için bu birleştirilebilir. **İkinci,
  ölçümle bulunan ince hata:** 1 Hz örnek + 1 Hz rapor **faz-kilitli değil**
  → bir rapor penceresi 0–2 örnek kapsayabilir; oranı rapor penceresi
  üzerinden (Δadım/rapor-Δt) almak sahte hız verir (loadgen `server_hz`
  58/198 Hz — tutarlı yeniden üretildi). Düzeltme: her örnek kendi
  `emit_at`'ını taşıyor (oda `Instant::now()`); oran **örnek aralığı**
  üzerinden (`latest.emit_at − prev.emit_at`), rapor penceresi üzerinden
  değil. Kanıt: `accumulator_applies_events_and_computes_rates` (emit_at 1 sn
  aralık → hz=30) + `loadgen_smoke` (server_hz 30.00, 3/3 stabil). Kapanan hata
  sınıfı: "gönderim temposu ≠ rapor temposu → atılan örnek" + "oran penceresi
  ≠ örnek aralığı → sahte hız". Elenen: (a) rapor penceresi oranı — faz
  kaymasında yanlış (ölçülen); (b) toplayıcının varış anı — varış≈emit ama test
  edilemez (test `Instant`'i kontrol edemez) ve iletme gecikmesi oranı kirletir;
  (c) örnekleri kanalda biriktirmek — örnek başına daha çok bellek; `emit_at`
  tek alanla yeterli.

- [x] **A3 — Metrik kanalı bounded + `try_send` (DESIGN §2 uyumu)** — Eskiden
  `mpsc::unbounded` ("geri-baskı fiilen sorun değil" diye, DESIGN §12); ama §2
  disiplini (paylaşımsız, sınırlı kaynak) ve gerçek bir toplayıcı sarkarsa
  unbounded kanalın bellek sızması riski. Yeni: `mpsc::bounded(4096)` +
  **senkron `try_send`** (Full → üretici `metrics_dropped` sayacını artırır,
  örnek atılır). `try_send` `send` gibi **senkron** (Future değil) → oda
  tick'ine await **eklenmez** (spec'in sert şartı korunur; bounded `send`
  Future olduğu için `send` değil `try_send` — `OutSink::flush` +
  `dropped_frames` ile aynı desen). Atılma zararsız (sayaçlar kümülatif; bir
  örnek kaybı oranı bir pencere bozar, birikimi değil). `metrics_dropped`
  rapor satırına eklendi (oda/registry/conn başına delta). Kapasite 4096
  (üç üretici × oda başına 1 örnek/sn × marj) — `start_inner`'de sabit.
  Kapanan hata sınıfı: "unbounded kanal sınırlı-kaynak disiplinini ihlal
  edebilir". Elenen: (a) unbounded (eski) — toplayıcı sarkarsa bellek sızması;
  (b) bounded + `send` (await'li) — oda tick'ine await ekler, spec ihlali;
  (c) `tokio::select!` (kanal+tick) — gsb-lint yasak; (d) toplayıcıda
  paylaşımlı ring-buffer — §2'yi bozar.

- [x] **A-ek — `snap_overflows` sayacı** — `max_snapshot_bytes` (vars. 1400)
  aşan snapshot sayısı, broadcast fazında sayılıyor (grup başına kodlanan
  paketin boyutu; grup bazlı, bağlantı bazlı değil). AOI'nin MTU sinyali:
  tüm-dünya snapshot'ı ölçekte her tick aşar, hücre başına snapshot aşmaz
  (aşağıdaki ölçüm doğruluyor). Rapor + loadgen RESULT satırına eklendi.

- [x] **B — AOI (görünürlük): `gsb-game` içinde, `gsb-core`'e dokunmadan** —
  **Ana sorunun cevabı: EVET.** Neden soyutlama sızması yok: çekirdeğin grup
  mekanizması AOI'nin gerektirdiğinin **tamamını** zaten sağlıyor —
  `RoomLogic::group_of(world, conn)` her tick yeniden değerlendiriliyor (hücre
  geçişi otomatik, yeni hücrenin bloğu), grup başına snapshot **bir kez**
  kodlanıp üyeyle `Arc` ref'iyle paylaşılıyor (§4), "değişiklik yoksa yayın
  durur" defter eşitliği **üyelik+konumu** içeriyor. AOI yalnızca mekansal bir
  `GroupKey` (`Cell(i32,i32)`, `gsb_game::aoi`) + hücre başına 3×3 komşuluk
  defteri sunuyor; `gsb-core` değişimi gerekmedi (bu turdaki gsb-core farkı
  %100 A maddeleridir — `git diff` ile doğrulandı). Alternatif (çekirdeğe
  `Visibility` trait'i / hücre kavramı eklemek) gereksiz soyutlama olurdu.
  **Görünürlük kümesi** = oyuncunun hücresinin merkezli **3×3 blok**
  (`RADIUS=1`; hücreler `floor(pos/cell_size)`). Oyuncu başına yarımçap
  REDDEDİLDİ: farklı küme başına → `GroupKey`'i `ConnectionId` yapmak gerekir →
  grup başına kodlama → bant kazancı sıfır + çekirdek değişikliği. **Hücre
  boyutu konfigüredir** (`Config.aoi_cell_size`, vars. 20.0) — sabit değil,
  çünkü doğru boyut yoğunluk/arena/MTU'ya göre değişir (aşağıda). Kimlik
  değişmezi korunur: wire id `on_join`/yeni `Position` damgasında `next_serial`
  ile **bir kez** basılır, hücre değişiminde DEĞİŞMEZ; sonradan giren kontrol
  fazında dünyaya girdiği için kendi hücresinin ilk bloğunda **tam kümesini**
  görür. Test kilitli: `gsb_game::aoi::tests` (5 mantık) + `tests/aoi.rs`
  (actor: fan-out, hücre geçişi, kimlik).

  **Ölçülen ticaret** (30 Hz, MTU 1400, in-proc, 15 s; Ryzen 9 7950X 16C/32T,
  rustc 1.95.0; 2000'de 1 ms stagger):

  | N | AOI | hücre | out/conn (bps) | toplam out | adım ort. (µs) | adım max (µs) | tepe payload (B) | aşım | grup |
  |---|-----|-------|----------------|-----------|----------------|---------------|------------------|------|------|
  | 500 | off | – | 118 869 | 59.4 M | 533 | 1 183 | 4 347 | 423 | 1 |
  | 500 | on | 20 | 94 788 | 47.4 M | 802 | 1 941 | 4 281 | 3 209 | 4 |
  | 1000 | off | – | 208 643 | 208.6 M | 1 535 | 5 071 | 8 210 | 443 | 1 |
  | 1000 | on | 20 | 185 787 | 185.8 M | 1 967 | 4 389 | 8 668 | 4 626 | 4 |
  | 1000 | on | 12 | 86 251 | 86.3 M | 1 479 | 4 817 | 6 811 | 5 167 | – |
  | 1000 | on | 8 | 64 064 | 64.1 M | 1 794 | 5 327 | 7 001 | 5 534 | – |
  | 1000 | on | 5 | 39 675 | 39.7 M | 2 077 | 3 468 | 5 717 | 4 824 | – |
  | 1000 | on | 3 | 7 001 | 7.0 M | 1 377 | 7 595 | 1 689 | 188 | – |
  | 2000 | off | – | 476 335 | 952.7 M | 6 192 | 11 453 | 17 699 | 445 | 1 |
  | 2000 | on | 5 | 80 972 | 161.9 M | 4 256 | 8 895 | 11 308 | 12 918 | 28 |

  **Yorum (ölçüm, varsayım değil):**
  1. **Kazanç hücre boyutuyla, ölçekte değil (bu iş yükünde).** Yarıçap-40
     halka 100×100 arenayı ~80×80'a sığar; c20 bloğu (60×60) halikanın
     ~%80'ini kavrar → bant kazancı küçük (c20'de %11-20). Küçük hücre
     (c5) bloğu küçük bir yayı kaplar → **%81-83** kazanç (1000/2000'de).
     Yani "büyük G'de kazanç" burada **mutlak** kazanç olarak geçer: toplam
     bant ~N² büyür (476 kbps/conn @2000) ve AOI'nin mutlak tasarrufu o
     ölçekte 952→162 MB/s'e çıkar; oran (hücre başına) ölçekte ~sabit.
  2. **MTU (1400) hücre boyutunu belirler.** ~12 B/kayıt → 1400 B ≈ 116
     kayıt/blok; blok 3×3=9 hücre → **~13 kayıt/hücre** hedef. c3 @1000
     tepe 1 689 B (hâlâ %13 aşım, 188/450 tick); c2 ~1400'ün altına iner.
     c20+ ölçekte **her tick** aşar (8 668 B). Yani MTU uyumu için hücre,
     N büyüdükçe **küçülmeli** (yoğunluk ∝ N).
  3. **AOI CPU maliyeti ~9× kodlama** (her kayıt 9 komşu bloğa girer) ama bu
     ölçekte bütçeye değmiyor: adım ort. 500'de 533 µs, 2000'de 6 192 µs (off) / 4 256 µs (c5) — bütçenin (33 333 µs) %6-18'i, `over_budget=0%`. **Break-even
     (adım → bütçe) N=2000'in ÜSTÜNDE**; doğrusal dışa vurumla (6 ms@2000 →
     33 ms) ~10-15k üye. 2000'in altında AOI (iyi hücreyle) **net pozitif**:
     büyük bant kazancı + bütçe altı CPU.
  4. **Yeni darboğaz: adım süresi (CPU = kodlama + fan-out), bant değil.**
     N büyüdükçe adım süresi ~N büyür (in-proc'ta istemci decode'üyle
     süper-lineer); bant AOI ile düşürüldüğü için ilk doyaan adım bütçesi
     olacak (~10-15k). Kötü hücre (c20) ölçekte **net kayıp** olur: 9× kodlama
     karşılığında yalnız %11 bant — CPU maliyeti büyürken kazanç sabit kalır.
     "Küçük G'de net kayıp" bu ölçekte (bütçe gevşek) görülmüyor; bütçe
     sıkışsa (yüksek tick hızı / dolu oda) 9× kodlama küçük G'de bütçeyi
     aşabilir.

## P0 — Ölçüm (önce veri, sonra optimize)

- [x] **Load test harness'i** — kapatıldı: `gsb-loadgen` binary'si +
  duman testi; 100/500/1000 ham sayılar, ilk doyma analizi ve burst
  bağlantı bulgusu "Kapatılanlar (metrik + yük turu)" bölümünde. 100k
  ölçeği hâlâ ölçülmedi (P2 — AOI/oda bölme sonrasına göre planlanır).
- [x] **Temel metrik** — kapatıldı: `gsb_core::metrics` +
  `MetricsCollector` (1 s rapor; log/kanal sink); kapsam ve tasarım
  "Kapatılanlar (metrik + yük turu)" + DESIGN §12'de.
- [ ] **`MovementSystem` unit testleri** — room-seviye testler dolaylı
  kapsıyor; spawn → target → run → konum/arrive doğrulaması hâlâ yok.

## P1 — Robustluk ve güvenlik

- [ ] **Oturum zaman aşımı** — ölü TCP bağlantısı (RST'siz kopma) slot +
  görev + kayıt işgal etmeye devam ediyor. Heartbeat son-görülme damgası
  + aralıklı süpürme (kanal mesajıyla, kilit yok).
- [ ] **`RoomConfig.max_players` + doluluk yanıtı** — `CoreError::RoomFull`
  zaten mevcut (bugün kurulmuyor — yayınlabilirlik turu MADDE 3
  taraması); doluysa JOIN'de `ERROR (room full)`.
- [ ] **Güvenlik yüzeyi** — `Authenticator` trait'i (AUTH bugün no-op),
  bağlantı sayısı limiti, aksiyon rate-limit.
- [ ] **Koordinat formatı kararı** — `sint32` (zig-zag varint, tam sayı) wire vs `f32`
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

İlk yük turu (100/500/1000 — "Kapatılanlar (metrik + yük turu)")
sıralamayı destekledi: N ile büyüyen tek metrik **adım süresi**
(fan-out maliyeti) ve registry'ye tek bir baskı sinyali gelmedi
(drop 0, late ≈0, bağlantı/oda olayları sönük). Sıra: AOI önce,
delta sonra; şeritleme veri gelmedikçe dokunulmaz.

- [~] **AOI / oda içi görünürlük** (`DESIGN.md` §8) — tek odada 100k
  bağlantı: 1.5 milyar frame teslimi/sn **CPU** duvarı. **Tek-oda mekansal
  AOI bu turda kapatıldı** (`gsb_game::aoi`, `GroupKey=Cell`, 3×3 blok; bant
  O(entity)→O(görünürlük); ölçüm + break-even "Kapatılanlar (metrik
  düzeltme + AOI turu)"). Kalan: oda segmentasyonu + `Visibility` trait'i
  (10k+ ölçekteki CPU duvarı — AOI'nin ~9× kodlama maliyeti ölçekte bütçeyi
  aşabilir, bkz. tur bölümü).
- [ ] **Delta yayın** — son snapshot farkı; bant kazancı.
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

## Bilinçli olarak yapılmayanlar (referans)

- Cross-server / cross-region, kalıcılık, yük dengeleyici — v1 kapsam dışı.
- bevy `Event`/observer sistemi hot path'te kullanılmıyor — bilerek
  (snapshot kararı wire içeriğiyle; ayrı bir versiyon bileşeni yok —
  eski `EntityVersion`/`bump()` denetim turunda kaldırıldı; gerekirse
  delta yayınında (P2) yeniden getirilir; `DESIGN.md` §7).
