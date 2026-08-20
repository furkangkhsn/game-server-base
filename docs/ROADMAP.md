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
(aşağıda, "Kapatılanlar (oda segmentasyonu turu)").
Test 58 → 83 → **97** (97/97 yeşil +1 var olan `#[ignore]`'li gsb-lint
doctest; hiçbir eski test silinmedi/ihmal edilmedi).
Aşağıdakiler **ölçülmemiş performans** (10k+ ölçek aşağıda ölçüldü;
kalanı çok makine dağıtımı, congestion control), **robustluk** ve
**güvenlik** başlıklarındaki kalan işler.

## Kapatılanlar (oda segmentasyonu turu)

Bu tur iki bölüm: **A — rUDP cookie key** (el sıkışma anahtarının
kaynağı; sessiz zayıf geri düşüşün kaldırılması — rUDP turunun
güvenlik devamı) ve **B — oda segmentasyonu (`sharded`)** (tek odanın
N shard actor'üne bölünmesi +
sınır migrasyon protokolü; §14.3/14.4'te "kaldıraç / katman
eklenmeden sığmıyor" olarak notlanan sınıfın ilk uygulaması).
Makine: AMD Ryzen 9 7950X 16C/32T (SMT), 124 GB, rustc 1.95.0, tokio
1.53.1. Yük koşuları **ayrı proses** (`--orchestrate --pin`), sunucu
8 fiziksel core'a pin'li (16 HW thread), istemciler ayrı process'ler;
koşular release.

### A — rUDP cookie key (OS entropisi)

**Problem.** rUDP el sıkışmasının cookie key'i proses başına **duvar
saatinden** türetiliyordu → yerel saldırgana (saat + pid bilen)
öngörülebilir → sahte proof üretilebilir. v1'de kriptografik katman
(HMAC) yoktu, ama key'in kendisinin tahmin edilemez olması, sahte-
proof/amplifikasyon korumasının *gerçek* dayanağı olmalıydı.

**Çözüm (seçilen).** Key, 16 bayt, iki kaynaktan biriyle kurulur:
(1) `Config.cookie_key: Option<[u8;16]>` verilmişse o (operatör
denetimi — deploy'da sabit anahtar isteyenler), (2) verilmediyse
`getrandom` ile **OS entropisinden** (`getrandom(2)`/`/dev/urandom`)
16 bayt. **Sessiz zayıf geri düşüş yok**: entropi kaynağı başarısız
olursa yapı `Result::Err` döndürür, süreç başlatmayı reddeder; duvar
saati gibi tahmin edilebilir bir değer asla kullanılmaz. `forged_
proof_is_rejected` testi: rastgele / sıfır / sabit key'li sahte
cookie reddedilir, oturum kurulmaz (+ 2 cookie akış testi).

**Elenen alternatifler.**
1. *Key'i duvar saati + pid'den türet, HMAC ekle*: HMAC katmanı ayrı
   bir protokol yüzeyi (imza doğrulama, key senkronu) demek; v1
   kapsamı dışında. Key'i entropiden almak aynı korumayı (tahmin
   edilemezlik) sıfır ek protokol yüzeyiyle verir.
2. *Key'i her oturumda istemciye sormak (pre-shared)*: stateless cookie
   modelini bozar (istemsiz oturum ön-oluşturulamaz), NAT reconnect
   akışını kırar; operasyonel yük (anahtar dağıtımı) gereksiz.

### B — Oda segmentasyonu (`sharded`)

**Ölçülen soruna (B0 karar koşusu).** C1 duvarı (tek oda, `all`,
ayrı proses): p50 adım 33,3 ms bütçeyi **9k-10k arasında** aşıyor,
10k'da `server_hz` 23,2'ye düşüyor. Soru: bu duvar çoklu oda ile mi,
yoksa tek odanın *içinde* segmentasyonla mı aşılır? Cevap (ölçümle):
**oyun sınıfına göre.**

- **Bölünebilir oyunlar** (MOBA takımları, ayrı lobiler, bölgeler):
  dünya zaten doğal odalara ayrışır → **çoklu oda yeter** (her oda
  bütçe içine sığar, bağlantılar odalara dağılır). Segmentasyon
  gerekmez, daha ucuzdur.
- **Sınırız tek dünya** (sürekli açık dünya, oyuncular bir dünyada
  dolaşır): oda sınırına takılamaz → **segmentasyon gerekir**
  (`sharded`). Bu tur bunu kurar.

Yani segmentasyon *tek sürekli dünya* sınıfına aittir; ölçüm, çoklu
odanın bölünebilir sınıfta yeterli olduğunu (2500 entity'li oda
adımının ~1 ms altı, bütçenin çok içi) ve tek dünyanın segment
gerekttiğini gösterir.

**Mimari seçim (neden `RoomLogic`'in altı değil, topoloji düzeyi).**
`sharded`, tek oda → N actor değişimidir; bu, `RoomLogic` trait'i ile
ifade edilemez (trait tek `World` + tek tick gövdesi varsayar).
Çözüm: `gsb_core::shard` (topoloji-agnostik shard actor + migrasyon
protokolü) + `gsb_game::sharded` (oyun-level `ShardLogic`: grid,
bölge, border, wire range) + registry `BuiltRoom::{Single, Sharded}`
(1→N actor topolojisi). Registry shard'ları **takip etmez**
(bağlantının şu anki shard'ını bilmez) — bu, shard'lar arası
kayb-ekleme (lost-update) durumunu önler.

**Tasarım kararları + elenen alternatifler.**

1. **Partition / AOI hizalama.** Shard'lar haritayı grid'e böler
   (`grid_shape`, en kareye yakın rows×cols, N≤16); bölge testi AOI ile
   aynı (`shard_at`). Elenmiş: (a) *dairesel/radial partition* — sınır
   uzunluğu ve komşuluk düzensiz, migrasyon yolu karmaşık; grid 4-komşu
   ile düzgün. (b) *hiyerarşik (quadtree)* — dinamik yeniden
   dengeleme gerekir, shard'lar arası migrasyon ağacı boyunca; grid
   statik ve basit (v1).
2. **Migrasyon — tek sahip değişmezi** (hiçbir tick dizininde entity
   iki shard'da da yok, hiçbirisinde de yok). Gönderen `Migrate`'i
   *başarıyla* `try_send`'ederse `t+1`'de despawn; başarısızsa mesaj
   geri alınır, bağlantı geri sarılır, entity kalır. Alıcı *install
   gate* ile `at_tick+1`'de spawn (erken gelen ertelenir). Elenmiş:
   (a) *write-through (her iki shard'da bir tick çift sahip)* —
   çift snapshot / çift migrasyon, kimlik çakışması; "tek sahip"
   değişmezi kırılır. (b) *2PC (prepare/commit)* — shard'lar arası
   await/round-trip, tick gövdesini senkron olmaktan çıkarır;
   `try_send` başarısı tek fazda aynı garantiyi verir.
3. **Sınır görünürlüğü (1-tick hizalama).** Komşular `BORDER` fazında
   tam durum değişir; shard komşunun sınır entity'lerini *borrow* olarak
   ekler (kendi kaydı 1-tick-geri borçlu kopyayı yener). Marj
   **çeyrek hücre** (`min(cell_w,cell_h)/4`). Elenmiş: (a) *tam hücre
   marjı* — dejener: s×s hücredeki her nokta bir kenardan ≤s uzakta
   olur, **bütün shard** export olur (borrow kümesi sınırsızlaşır).
   (b) *sıfır marj (sadece sınır hücresi)* — sınırın iki tarafını
   kapatmaz, entity sınırda görünmez olur (lag-blink yerine kayıp).
4. **Bağlantı sahipliği.** Migrasyonda **kanallar entity'yle gider**
   (`Migrate` mesajı `out`/`in` mailbox'larını taşır); yeni shard
   snapshot'ı doğrudan o bağlantının writer'ına yollar. Elenmiş:
   (a) *registry'nin shard'ı takip etmesi (bağlantı → shard yönlendirmesi
   migrasyonda güncellenir)* — shard'lar arası güncelleme = kayb-ekleme
   (bounded kanal + eşzamanlı migrasyon/leave); registry'nin shard'ı
   bilmemesi bu sınıfı tamamen ortadan kaldırır. (b) *bağlantının
   shard'lar arası "forward" etmesi* — her snapshot shard → registry →
   shard round-trip, ek gecikme + kanal.
5. **Wire kimliği (range partitioning).** Shard `i` → `i * 2^20 +
   serial` (`SHARD_SERIAL_RANGE = 1<<20`); id migrasyonda değişmez
   (entity `WireId`'siyle gider). Elenmiş: (a) *global (paylaşımlı)
   id sayacı* — join senkron tick gövdesinde koştuğu için global "sonraki
   id" shard'lar arası senkronizasyon ister; range partitioning sıfır
   senkronizasyonla çakışmasızlık verir. (b) *her shard kendi 0'dan
   sayar (id + shard ön eki)* — istemci kimliği shard'la bileşke
   olur, migrasyonda id değişir (istemci dünyasında kimlik bozulur).
6. **Leave / migrasyon yarışı — iki-tablo epoch şeması.**
   `conn_epoch` = shard'ın şu an tuttuğu join'in epoch'u (Migrate-out
   epoch taşımak için); `conn_tombstone` = işlenen en yüksek LEAVE
   epoch'u. Kapı **tombstone**'a bakar (`tombstone >= p.epoch` →
   reject), *kurulu* epoch'a değil. Elenmiş: (a) *kurulu (`conn_epoch`)
   tablosuna kapı* — ping-pong migrasyonu aynı join'in epoch'unu taşır;
   kurulu tablo onu "bilinen" sayıp **canlı join'i reddederdi** (test 1,
   tick 14'te "wire 1 owned by []" ile yakalandı). (b) *registry'nin
   leave'i shard'a iletmesi (yönlendirmeli)* — registry'nin shard'ı
   bilmesi gerekir (karar 4'e aykırı); leave'in **tüm** shard'lara
   `max` birleşmesiyle yayılması (epoch 0 ile, zararsız) daha basit ve
   kayıpsız.
7. **`&World` metotları → cache.** `collect_border`/`own_wires` `&self`
   + `&World` aldığı için (borrowed context) canlı sorgu yapamaz
   (`&mut World` isterlerdi) → `border_cache` (update sonunda yeniden
   kurulur; migrate-out'a karşı 1-tick-geri **zararsız**, alıcının
   `own_wires` filtresi bayat kopyayı düşürür) + `own_wires: HashSet`
   (her mutasyonda senkron — bayat kayıt, komşunun artık-düzgün
   kaydını gizleyip entity'yi 1 tick düşürür).
8. **Kapasite.** Cap oda düzeyinde **tama** (`members + pending >=
   cap`); shard başına `ceil(cap/N)` değil (range-partitioned join,
   cap'in dağılımını bilmez).

**Doğrulama (unit + entegrasyon; hepsi yeşil).**
- `gsb_core::shard` (5 test): `migration_never_drops_or_duplicates`
  (ping-pong, her tick dizininde tam-tek sahip + süreklilik),
  `wire_identity_stable_and_disjoint` (id stabil + shard'lar arası
  çakışmasız), `in_flight_action_survives_migration` (uçuştaki aksiyon
  migrasyonda kaybolmaz), `ghost_migrate_after_leave_is_rejected`
  (iki-tablo epoch: leave sonrası hayalet migrasyon reddedilir),
  `boundary_entities_are_visible_to_both_sides` (sınırda iki oyuncu
  birbirini görür).
- `gsb_game::sharded` (6 test): `region_partition_tiles_the_map`,
  `wire_ranges_are_disjoint_and_stable`,
  `migration_reports_crossing_with_full_state`,
  `border_visibility_across_the_seam`,
  `frame_filter_discards_far_neighbor_edges` (N=16: sınır entity'si
  görünür, uzak-kenar entity'si frame filtresinde elenir),
  `snapshot_union_and_no_change`.
- Loadgen: `--visibility sharded --shard-count N` (in-proc + `--serve`
  + `--orchestrate` üç modda da shard_count akıtıldı); rapor
  **shard-aware** (`fold_rooms`: tek oda = özdeş, sharded = shard'lar
  üstü toplam/en-kötü-shard) + RESULT'a `shards=N` alanı.

**Ölçüm (ayrı proses, `all`, 30 Hz, sunucu 8 core pin'li, 10000
bağlantı, stagger'lı join — temiz steady-state):**

| Konfig | peak_conns | server_hz | adım p50 | adım max | over-budget | server_cpu_s |
|---|---|---|---|---|---|---|
| C1 (1 oda, `all`) | 10 000 | 30.02 | 6 250 µs | 73 279 µs | **%3,4** | 80,8 |
| `sharded N=4` | 10 000 | 29.93 | 6 250 µs | 72 956 µs | **%0,2** | 111,4 |
| `sharded N=8` | 10 000 | 29.93 | **782 µs** | 35 818 µs | **%0,0** | 117,8 |

8k'da fark daha belirgin: C1 `over_budget %7,7` / `step_max 146 831 µs`
(4,4× bütçe) vs `sharded N=4` `%0` / `31 305 µs`.

**Yorum.** (1) **Adım-duvarı (tick bütçesi) segmentasyonla kayıyor:**
tek oda 10k'da bütçeyi aşarken `sharded` aşılmıyor; N arttıkça p50
adım düşüyor (N=8 ~8×). (2) **Fiyat:** shard protokolü (kanal trafik +
border değişimi + migrasyon + N× actor) sunucu CPU'yu ~1,4-1,5×
artırıyor (80,8 → 117,8 core-s; pin'li havuzun ~%22 → %32'si) — sunucu
hâlâ doygun değil, dolayısıyla CPU henüz yeni duvar değil. (3)
**Fan-out/decode duvarı segmentasyonla kaymaz:** `dropped`
(snapshot'ların istemci kanalında atılması) üç konfigde de yüksek
(2,3-4,0M) ve istemci decode'u ile sınırlı (`clients_cpu_s≈400`) — bu,
loadgen istemcisinin 10k×30Hz×~60KB snapshot'ı decode edememesidir,
sunucu duvarı değil. Fan-out duvarını kaydıran **mesafe-yanlı
görünürlük**tür (§8.1), segmentasyonla *birleşir*. (4) **Yeni duvar
konumu:** 10k (oda cap) sınırında adım-duvarı aşılmıyor; bir sonraki
duvar oda cap'i / bağlantı-altyapısı, adım değil. Segmentasyon tek
sürekli dünya sınıfına aittir; bölünebilir oyunlarda çoklu oda daha
ucuz aynı işi yapar.

**Kapsam notu / yapılmayanlar.** Shard'lar **tek makine** içindeki
actor'lardır (çok makine dağıtımı — shard'ların network'e yayılması —
yapılmadı, §14.4). Shard sayısı 1..=16 (grid sınırlı). Sınır
entity'si en fazla 1 tick geri görünür (lag-blink: ≤1 tick yok, asla
kopyalanmaz) — kabul edilen dejenerasyon. Migrasyon sayaçları
`RoomSample`/`RoomReport`'a **eklenmedi** (kapsam kontrolü; yalnız
`tracing` logları).

## Kapatılanlar (ihlal bütçesi + rUDP turu)

Bu tur iki bölüm: **A — protokol-ihlal bütçesi** (cevap amplifikasyonu
sınırı; istemci geliştirici tanısı) ve **B — rUDP taşıması**
(DESIGN §6'daki "aktör katmanında değişiklik sıfır" iddiasının
yapısal sınavı: UDP'de "bağlantı" bir socket değil, datagram
akımlarından sentezlenen oturumdur). Makine: 32 core / 124 GB /
rustc 1.95.0 (koşular release; in-proc yük ölçümleri istemcilerle
server'ı aynı CPU'da paylaşıyor).

### A — Protokol-ihlal bütçesi

Spec'in iki sorusu: (1) amplifikasyon sınırı nasıl konur, (2) istemci
geliştirici neden reddedildiğini anlasın. Cevap: **bağlantı
actor'ünün YEREL durumunda** bir sayaç (kanal yok, görev yok,
registry bilmez; bağlantı ölür sayaç ölür):

- **Bütçe: ömür boyu toplam 16 ağırlıklı puan** (sınıf ağırlıkları:
  Hard = 4, Race = 1, sunucu tarafı koşullar = 0). *Neden kayan
  pencere değil:* ilgili pencere bağlantı ömrüdür — istemci ya
  düzelir ya ölür; churn yapan istemcinin penceresi zaten sıfırdır
  (yeni bağlantı = yeni bütçe, bu zaten yeniden-auth'un doğal
  sonucu). Kayan pencere = bağlantı başına zamanlama durumu +
  per-tick iş — sıfır kazanç.
- **Ağırlık sınıfları:** Hard (bilinmeyen opcode, decode hatası,
  auth ihlali = 4) → 4 ihlahta kapanır; Race (NotInRoom = 1) →
  meşru yarışlar (oda tick sınırında ölüm, leave→rejoin) bütçeyi
  neredeyse çizmez (16 race gerekli); sunucu tarafı koşullar
  (cap/dolu oda = 0) → istemcinin suçu değil, hiç sayılmaz.
- **İlk 3 cevaplanır, sonra susulur:** istemci geliştiriciye 3
  hata mesajı gider (tanı yeterli), sonra ERROR hunisi susar —
  amplifikasyon sınırlı kalır (50 reddedilen istemci = 150 ERROR
  toplamı; sınırsızda 1550+ olurdu; ölçüm: 150 istemci load'da
  `errors=150`, tur öncesi sınırsız davranış ~4×'i).
- **Kapanma:** bütçe tükenince ERROR 9 + mesajda "violation" +
  temiz cleanup kaskadı (mevcut huni — yeni kapatma yolu yok).
- **Peer adresi TAŞINIR:** `Endpoint::peer()` → connection
  actor'ü → kapanma WARN satırı (`self.conn=… self.peer=…`) —
  operatör satırı firewall/fail2ban'a doğrudan verebilir; v1'de
  ban listesi yok (spec bunu kapsam dışı ilan etti — burada yalnız
  SİNYAL kuruldu, KULLANIMI kurulmadı).

**Elenen alternatifler:** (a) kayan pencere (yukarıda); (b) eşit
ağırlık (race'ler Hard ile aynı ağırlıkta → meşru churn'li
istemciler bütçeyi eritir, saldırganla ayrımı zayıflar); (c) her
ihlale cevap (amplifikasyon = spec'in başlıca sorusu, sınırı
yok); (d) adresten yoksun sinyal (operatör elini kolunu bağlar —
peer zaten actor'de, taşıma bedelsiz).

**Ölçüm (150 istemci, room-full, in-proc, release):**
`joined=100 … errors=150 join_rejected=50 cap_rejected=0
budget_rejected=50 actions_dropped=0`; kapanma kaydı
kendine yeter: `closing connection: protocol violation budget
exhausted self.conn=c64 self.peer=127.0.0.1:52116 violations=16
answered=3 score=16`.

**Spec düzeltmesi (not):** spec, bilinmeyen opcode'ların zaten
sınırsız ERROR ürettiğini varsaydı; gerçek actor'de kayıtsız
**temel bant** op'leri (10..999) odaya FORWARD ediliyordu —
buda turda `UnknownOpcode` dalı eklendi (10/11 = UDP taşıma
işaretleri belgeli; TCP'de bunlar bilinmeyen op = bütçeli).

### B — rUDP taşıması

**Yapısal fark:** TCP'de `accept()` kernel'in bitirdiği bir
bağlantıyı teslim eder; UDP'de bir soket **tüm** oturumları
taşıyor, dolayısıyla "accept" datagram akımından oturum
sentezlemektir. Seçilen topoloji (DESIGN §6'da detaylı): **tek
ortak demux görevi** (listener'ın, `bind()`'de başlatılır) tüm
datagramları okur ve peer adresiyle per-oturum mailbox'lara
route eder; her oturum yalnız **writer** görevi taşır (reader
yok). `accept()` = el sıkışma tamamlanınca demux'un ön-oluşturduğu
oturum endpoint'lerinin (crossbeam bounded(1024)) akışı.

**100k matematiği (seçilen demux):** datagram başına = 1
`recv_from` syscall + 1 hash lookup + 1 `BTreeSet` insert
(O(log 100k) ≈ 17 adım) + 1 `try_send` ≈ 0,5 µs → 100k oturum
× 2 datagram/sn ≈ **0,1 core**; oturum başına 10 datagram/sn
(`all` görünürlüğünün tavanı; 3M datagram/sn) ≈ **1,5 core** —
TCP'nin per-connection writer syscall yüküyle aynı mertebeye.
Asıl 100k duvarı çekirdeğin tek-socket pps'si + fan-out hacmi
(görünürlük) — TCP ile aynı sınıf, başka değil.

**Elenen alternatifler (matematik):**
- **SO_REUSEPORT shard'leme** (N soket, N demux, kernel hash):
  tek demux CPU'sunu ~1,5 → 1,5/N core'a indirir — ama toplam
  pps tavanı aynı kaldığı için kazanım **%5'ten az**; ve üç yapı
  kırılır: tek `accept()` akışı (shard başına accept = registry'nin
  tek-accept akış modeli), ortak monoton `ConnectionId` alanı
  (shard başına sayacı = kimlik değişmezinin kırılması), ortak
  idle heap (shard başına saat).
- **Per-due O(N) sweep** (deadline'a kadar bekle, tabloyu tara):
  heap'in O(log N) insert/remove'u yerine O(N) tarayış — join/
  leave churn'ında (100k'da her join/leave bir girdi) 80× maliyet;
  ayrıca "sıradaki due"yu bulmak için de tarama gerekir.
- **`Listener::accept` içine gömülü demux:** `accept`'in döndürdüğü
  future, demux'un kendisini (soketi) borçlanması gerekir →
  self-referential future → 'static stream için unsafe/mio
  seviyesinde iş — kanal yasağı + `unsafe_code="forbind"` ile
  çıkmaz.

**Zorunlu özellikler (hepsi yapıldı):**
1. **El sıkışma cookie/token:** stateless 3-message cookie
   (`HELLO{nonce,0}` → `HELLO{nonce,F(nonce,peer,key)}` →
   `HELLO{nonce,cookie}`); `F` = splitmix64 katlaması, key proses
   başına duvar saatinden. Oturum durumu **yalnızca** son mesaj
   doğrulanınca kurulur (asıl hedef: handshake amplifikasyonu —
   sahte proof key bilmeden üretilemez; oran ≤ 1 çünkü iki
   yöndeki mesaj aynı boyutta).
2. **Band ayrımı:** kontrol bandı (AUTH/JOIN/LEAVE/HEARTBEAT =
   `op 1..=64`, 11 hariç) REL: `[u32 seq][u16 op][payload]` +
   cumulative ACK + RTO 50 ms yeniden gönderim (vazgeçme 250 ms,
   sayılır) + 16'lık out-of-order penceresi, **sıralı teslim**.
   Oyun bandı (`op ≥ 1000`) RAW: `[u16 op][payload]` — kayıp
   toleranslı, sırasız (snapshot'lar tam durum; MOVE_TO'nun kaybı
   bir sonrakiyle örtülür).
3. **MTU:** `max_datagram_bytes` varsayılan 1472 (1500−20 IP−8
   UDP). Bütçe üstü **çıkan** datagram: **atılır + sayılır +
   oturum başına tek uyarı**. *Neden parçalama değil:* parçalama
   bir protokol (assembly penceresi, frag timeout, parça kaybında
   tüm fragment'ı bekleme) — v1 kapsamı (aşağıda); *neden ret
   değil:* ret, gönderen aktöre geri sinyal gerektirir (yeni
   kanal/yön) ve canlı oturumu kırar; drop+count, drop'un nerede
   olduğunu sayılarla gösterir (loadgen'de `oob_dropped`, oda
   tarafında zaten `snap_overflows`).
4. **Oturum kapatma (FIN yok):** demux'un
   `BTreeSet<(Instant, SocketAddr)>` deadline heap'i (gömlekli
   geçersiz kılma: girdi yalnız `son_görülme + idle`'e eşkenken
   geçerli — last_seen güncellenince eski girdi self-expire olur)
   + `timeout(min_deadline, recv_from)`; sweep oturuma
   `ConnIn::ServerClosed` yollar + oturumu kaldırır. Her frame
   last_seen'i sıfırlar (TCP'nin reader-pump saatine birebir
   karşılık). `idle_timeout` config'den (vars. 30 sn).

**Trait sızması (DURUMU VE NEDENİ — dürüst cevap):** `gsb-core`
**dokunulmadı** (odanın tek await'i `tick_rx.recv()`, connection
actor'ün tek await'i `inbox.recv()`, registry oda asla await etmez —
hepsi korundu). Sıza `gsb-net` trait şeklinin **4** yerinde
oldu, her biri yapısal (alternatifi yoktu):
1. `Endpoint::take_inbox`/`take_outbox` (+`with_inbox/with_outbox`):
   demux, oturum mailbox'larını el sıkışmada — accept loop
   çalışmadan **önce** — kurmak zorunda (datagramlar o anda
   gelebilir); TCP'de reader, accept'in içinde kurar.
2. `start_pump`/`PumpSpawner` artık `(Option<JoinHandle>,
   JoinHandle)` döndürür: UDP'de per-connection reader yoktur
   (ortak demux); reader handle'ı `Option`.
3. `Listener::close(&self)` (§9'un ertelenmiş kapısı kullanıldı):
   demux tüm bağlantılardan uzun yaşar — `Arc`'lerin düşmesi onu
   durdurmaz; `stop()` onu abort eder.
4. Peer adresi endpoint'ten aktöre taşınır (`Endpoint::peer()` →
   `ConnectionActor.peer`) — ihlal sinyali için (Bölüm A).
   gsb-server tarafı: `Config.transport` + `udp_max_datagram_bytes`
   + **`crossbeam-channel` bağımlılığı** — `Endpoint`
   göndericisi/`Receiver`'ı `Arc<dyn Listener>`'den
   `recv`/`try_recv` **`&self`**'den çalışmalı (tokio mpsc'nin
   `recv`'i `&mut` ister; kilit eklemek yasak, `unsafe` yasak →
   crossbeam'in atomik iç yapısı tek yol; tek kullanış noktası bu
   akış).

**Kabul kriteri — e2e aynı niyetle her iki taşımada:** `e2e.rs`
`Client`/`Recv` soyutlaması üzerine yeniden yazıldı; 7 akışın
hepsi `for kind in [Tcp, Udp]` ile koşuyor (7 test × 2 = 14 akış,
test sayısı 7). UDP tarafının niyet farkları belgeli: "kapandı"
EOF ile değil **probe başarısızlığı** ile kanıtlanır (HEARTBEAT →
1,5 sn içinde HEARTBEAT_ACK yok = kapandı); `recv` penceresi
"sessiz pencere" = `Ok(None)` (hata değil — UDP'de EOF yoktur).

**Ölçüm (loadgen `--transport udp`, 150 istemci, 10 sn, in-proc,
release, `all` görünürlük, max_players 100) — TCP karşısına:**

| metrik | TCP | UDP |
|---|---|---|
| connected / joined | 150 / 100 | 150 / 100 |
| connect p50 / p99 | 27 / 55 ms | **19 / 35 ms** (handshake) |
| snapshot toplamı | 29 159 | 29 100 |
| measured hz (medyan) | 30.06 | 30.01 |
| adım mean / p50 / max | 81.3 / 130 / 257 µs | 100.5 / 130 / 402 µs |
| adım bütçesi aşımı | 0 % | 0 % |
| client in (ort) | 2 217 KB/s | 2 248 KB/s |
| server out (ort) | 2 243 KB/s | 2 239 KB/s |
| out_bps_per_conn | 15 314 | 15 286 |
| late_max / dropped / actions_dropped | 3 091 µs / 0 / 0 | **1 148 µs** / 0 / 0 |
| istemci yeniden gönderimi (retrans_out) | — | 769 / 10 sn |
| dup_in / oob_dropped / gave_up | — | 0 / 0 / 0 |

Okuma: snapshot bant genişliği ve tick hızı eşdeğer (aynı oda);
handshake TCP connect'inin **altında**; adım mean/max UDP'de
biraz yatakta (demux + per-session writer, in-proc CPU paylaşımı
altında — 4 adım p99 binine; bütçeyi aşan yok); late_max UDP'de
**3× daha iyi** (1 148 vs 3 091 µs — demux'un tek okuyucusu, TCP
reader'larının per-connection wake-up'ı yok); loopback'te sıfır
kayıp; 769 yeniden gönderim = in-proc baskı altında RTO'nun
görevini yapması (kontrol bandı sıralı teslimi bozmadan
kurtardı).

**v1 kısıtları (kapsam dışı — burada ilan, inşa yok):**
congestion control (UDP'de pps sınırını yalnız oda bütçesi
tutuyor; P1: token bucket), şifreleme (cookie key'i duvar
saatinden — yerel saldırgana öngörülebilir; P1: DTLS/TLS katmanı),
parçalama (bütçe üstü = drop+count; `snap_overflows` sinyaliyle
grup bölme asıl kaldıraç — §8), SO_RCVBUF ayarı (tokio 1.53.1
setter'ı yok), NAT yeniden bağlanması = yeni el sıkışma + yeni
`ConnectionId` (eski oturum ≤ `idle_timeout` yaşar), oturum
actor ölümünden sonra ≤ `idle_timeout` kadar demux'te yaşar
(demux'un `in_tx` + registry klonu kanalı canlı tutar; idle sweep
temizler).

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

## Kapatılanlar (görünürlük stratejileri turu)

Bu tur "görünürlük"ü sunucu seçimi yapar: aynı oyun (aynı component'ler,
aynı hareket sistemi, aynı wire format) üzerinde **üç değiştirilebilir
görünürlük stratejisi** + taban (`all`), hepsi aynı `RoomLogic` seam'i
üzerinde; strateji `Config.visibility` ile seçilir (sunucu + `gsb-loadgen`).
Ayrıca: soyutlama kararı (C) ve D1–D3 yük ölçümleri. Test: 44 → **58**
(+14: 6 team + 6 pvs mantık testi + 2 actor-level; hiçbiri `#[ignore]` değil,
eski 44'ün hiçbiri değiştirilmedi).

- [x] **A1 — Mekansal/MMO (`AoiRoom`, `GroupKey=Cell`)** — bu turda
  davranışsal değişim yok; karşılaştırma tabanı olarak korundu (kod
  yalnızca C maddesiyle ortaklaştırıldı, bkz. aşağı).

- [x] **A2 — Takım sisli / MOBA (`TeamRoom`, `GroupKey=Team`, 2 grup)** —
  Bir takımın paketinde düşman entity, o takımın **görevli kaynaklarından
  en az birinin menzilindeyse** yer alır. Seçilen çözüm: takımlar `conn.id`
  paritesinden gelir (`team_of`); **grup anahtarı world okumaz** (konum
  bazlı gruplama `group_of`'u world'a bağlardı — group-snapshot'ın
  "grup = bağlantı sınıfı" sözleşmesiyle çelişir); menzil
  `team_vision_radius` konfigüredir (vars. 25.0). Görevli kaynak modeli:
  **her oyuncu entity'si eşit yarıçaplı görevli kaynağıdır**; sahibi olmayan
  (orphan/stamp'lı) entity **her iki takıma da** yayılır (yayınlanabilir
  küme = "Position taşıyor" değişmezini bozmaz). Aynı tick'te **tek kodlama
  / takım**: 2 snapshot, `Arc` ref'le paylaşım — 2 takımın paketi
  aynı-tick üretimi olduğundan istemci taraflı asimetri testi anlamlıdır.
  Kapanan hata sınıfı: "görünürlük client'a emanet" — hile testi
  (`tests/team.rs`): B takımı üyesi (10,0)'deyken A takımı üyesinin (40,0)
  snapshot'ı **hiç** düşman wire-id'ini taşımazken, B'nin snapshot'ı
  taşır (aynı tick, aynı dünya); entity görevliye girince **aynı wire id**
  ile var olur, çıkınca snapshot'tan düşer (istemci "kayboldu" okur —
  self-contained sözleşme; delta/öncül-olay yok). Elenen: (a) **ward/sahipli
  görevli componenti** — spec'teki "unit ve/veya ward" bu turun "aynı
  component seti" çerçevesinde yeni component gerektirir; `VisionSource {
  radius, owner_team? }` gibi bir component stratejiyi ifade edebilirdi ama
  tur çerçevesi (yeni oyun mantığı eklememek) bunu dışarıda tuttu — eşit
  yarıçaplı oyuncu-kaynak modeli aynı testleri geçirir, ward desteği
  component eklemekle geri getirilebilir (sözleşme değişmez: "kaynak
  menzilinde"); (b) **bağlantı başına grup** (`GroupKey=ConnectionId`) —
  her istemcinin menzili farklı olsa bile takım sisli menzil
  **takım-bazlı** olduğundan bağlantı başına grup hem 2 grubu bozar hem
  grup başına paylaşılan baytı (asıl kazanç) sıfırlar; (c) **konum bazlı
  takım** — takım bir oyun durumudur, konumdan türetilmez (harita yarısı
  takımla takım sisli olmaz).

- [x] **A3 — PVS/FPS (`SectorRoom`, `GroupKey=Sector`)** — Harita elle
  yazılmış **4 kavisel (convex) sektör** + **statik görünürlük tablosu**
  (`VISIBLE_FROM[S]`: bit mask'ı; A↔B arası duvar, kuzey bandı C/D ile
  açık); `sector_of` = kavisel test (cross product işaretleri, CCW
  çokgen); harita dışı konum `OUT` catch-all sektörü (yayınlanabilir küme
  değişmezini harita sınırı bozamaz). Grup başına snapshot = görünürlük
  tablosundaki sektörlerin bucket birleşimi; hücre geçişi `group_of`
  yeniden değerlendirmesiyle otomatik, **wire id değişmez** (test kilitli:
  `tests/pvs.rs`). Kapanan hata sınıfı: "mesafe testi görünürlüğü
  kanıtlamaz" — 3 birim aradaki iki entity **bağlantısız** sektörlerde
  birbirini GÖRMEZ (A(-1,0) ile B(2,0): 3 birim, duvar → ayrık snapshot'lar);
  bağlantılı sektörler (A↔C) görür; sektör geçişinde aynı id taşınır.
  Elenen: (a) **BSP/oklüzyon derleyicisi** — spec açık: "BSP derleyicisi
  yok"; elle convex sektör + tablo bu arenada (100×100, 4 bölge) derleyici
  maliyetini gerektirmez, tablo statik olduğu için tick'te sıfır maliyet;
  (b) **tick başına ray-cast / mesafe testi** — O(entity²) ışın atımı ve
  "3 birim mesafe = görünür" yanılgısını (hata sınıfının kendisi) üretir;
  duvar/koşu koridoru ayrımı mesafede kodlanamaz; (c) **sektör = AABB
  kesişimi** — kavisel olmayan bölgede "görünürlük ⊆ mesafe" garantisi
  bozulur (AABB köşeleri birbirini görmeyen bölgeyi görünürlük alanına
  sokar); convex + tablo, "tablodaki sektör kümesi = kesin görünürlük
  alanı"ı elle doğrulanabilir yapar.

- [x] **B — Strateji seçimi (sunucu config + loadgen) + varsayılan** —
  `Config.visibility: all | spatial | team | pvs` (serde lowercase;
  `config.example.toml` belgeli); `gsb-loadgen --visibility …` aynı seçimi
  yapıyor (`--cell-size`, `--vision-radius` strateji parametreleri).
  **Varsayılan: `all`.** Gerekçe (üçlü): (1) **geri uyumluluk** — önceki
  tüm yük ölçümleri `all` (eski `aoi=false` taban) üzerinden alındı;
  varsayılan değiştirse eski sayılarla karşılaştırma kırılır; (2) **ölçüm
  tabanı** — `all` en kötü durum (en çok kodlama + bant) olduğundan her
  stratejinin kazancı/bedeli onun karşısında net okunur; (3) **en az
  sürpriz** — görünürlük kısıtlama bir **oyun kararıdır** (takım sisli
  istemeyen bir MMO, PVS istemeyen bir MOBA); sessizce kısıtlanmış
  görünürlük bug olarak algılanır, açık seçim ise özellik. Elenen:
  `spatial` varsayılanı (en yaygın MMO profili argümanı — ama 9× kodlama
  maliyetini varsayılan yapar ve önceki taban verisiyle kesintisiz
  karşılaştırmayı bozar; hücre boyutu yanlış seçilirse net kayıp — ölçülen,
  bkz. D2).

- [x] **C — Soyutlama kararı: `Visibility` trait'i GEREK YOK; `RoomLogic`
  yeterli seam. Ortak mekanik `gsb_game::common`'e taşındı.** Karar,
  üç strateji yazıldıktan sonra koddan:
  - **Trait gerekmez** çünkü stratejiler arasındaki gerçek fark,
    `RoomLogic`'in zaten ayırdığı iki şeydir — `GroupKey` tipi +
    `group_of`/`snapshot` içeriği. Bir `Visibility` trait'i aynı farkı
    yeniden soyutlar; üstelik stratejilerin içerik hesapları
    (küme birleşimi / mesafe önelemi / statik tablo) ortak bir imzaya
    sığacak kadar da benzer değildir — trait'in arkasında duracak ortak
    bir uygulama yoktur. Kanıt: bu turda 2 yeni strateji + 1 yeni metrik
    metodu eklendi, `gsb-core`'ün trait **şekli değişmedi** (tek core
    değişimi ekleyici: `RoomLogic::encoded_records()` default metodu +
    `snap_records` sayacı — D3 ölçümü için; core'un test logic'leri
    default 0 ile davranışsal değişim yok).
  - **Ortak olan gerçek şey** strateji değil, oda **muhasebesi**: runner
    kurulumu, `conn_entity` tablosu, wire sayaç + minting, `on_join` /
    `on_leave` / `ingest` (MOVE_TO decode + guard'lar), sistem yürütme,
    orphan stamp'leme — 4 odada **birebir aynı ~45 satır** (4 kopya).
    Bunlar `gsb_game::common` modülüne tek kopya olarak taşındı (traitsiz
    düz fonksiyonlar; odalar kendi alanlarını korur — inline testler
    `room.conn_entity` vb.'ye erişiyor, 44 eski test satırı bile
    değişmedi). `common::next_serial` artık `WireId::new`'ün **tek
    çağrıcısı** (mint noktası tek yerde; `components.rs` + `room.rs`
    doküman referansları güncellendi). Bir 5. stratejinin maliyeti ~250 →
    ~80 satıra düştü (sadece `GroupKey`, `group_of`, `snapshot` + içerik
    hesabı).
  - Elenen: (a) `Visibility` trait'i (yukarıda); (b) `RoomBook` struct'ı
    (alanları sahiplenen ortak struct) — odaların alan adlarını değiştirir,
    44 eski testin içindeki `room.conn_entity` erişimini kırardı; test
    silme/değiştirme yasak olduğundan fonksiyon-over-alan deseni seçildi;
    (c) "hiçbir şey yapma" — 4 birebir kopya gerçek kod; "spekülatif değil"
    kriteri bu duruma tam uyar (dört kopya mevcut, beşincisi gelebilir).

- [x] **D1 — Mekansal AOI break-even (ölçüm, tahmin değil)** — 30 Hz,
  release, in-proc (istemciler sunucuyla CPU paylaşır → muhafazakâr
  alt sınır), hücre 5, stagger 1 ms, 15 s; makine: AMD Ryzen 9 7950X
  16C/32T, 124 GB RAM, rustc 1.95.0; masaüstü paylaşımlı (arka plan
  ~2-3 çekirdek: argos/thunderbird vb. — 1 dk loadavg ~9-11/32).

  | N | adım p50 (µs) | adım max (µs) | bütçe aşımı | drop | late tick max | tepe payload (B) | aşım (1400 B) | grup | out/conn (bps) |
  |---|--------------|---------------|-------------|------|---------------|------------------|---------------|------|----------------|
  | 2 000 | 6 250 | 21 737 | %0 | 0 | 42 µs | 11 372 | 12 895 | 26 | 80 991 |
  | 4 000 | 12 500 | 27 040 | %0 | 0 | 2 010 µs | 19 645 | 25 127 | 35 | 127 464 |
  | 6 000 | 25 000 | 55 565 | %3,8 | 0 | 42 290 µs | 25 136 | 32 033 | 48 | 146 978 |
  | 8 000 | 25 000 | 60 305 | %12,2 | 3 869 | 271 745 µs | 26 892 | 37 090 | 76 | 148 088 |
  | 10 000 | 12 500 (bimodal) | 86 862 | %18,9 | 54 263 | 1 343 687 µs | 23 570 | 39 370 | –* | 108 267 |

  *n=10 000 son raporu istemcilerin ayrılma (drain) sırasında örneklenmiş
  (groups=1, members=1); tüm istemciler join etmiş (connected=joined=10 000).

  **Cevap (ölçülen, tahmin yok):** p50 adım süresi **N≤10 000'de 33,3 ms
  bütçeye ulaşmıyor** (10 000'de p50 12,5 ms — bimodal: çoğu adım hızlı,
  kuyruk yavaş). Önceki turun ~10-15k **tahmini** böylece "bütçe p50'de
  10k'ın üzerinde" olarak daralıyor; ama p50 tek doğru gösterge değil:
  **bütçe aşan adım oranı 6k'da %3,8 → 10k'da %18,9**, max 86,9 ms (bütçenin
  2,6×), ve 10 000'de **54 263 batch drop + 1,34 s late tick** — oda
  actor'ünün ticker'dan geride kalması (catch-up adım patması; ticker'ın
  kendisi burst yapmaz — `ticker.rs` resync eder, ölçülen hz=42,5
  room-yana catch-up'un izi). 2k-8k arası 1 dk loadavg ~9-11/32 olduğuna
  göre makine bütünü doymuş değil; doyan parça **in-proc istemci decode
  yolu** (10 000 istemci × ~20 KB/snapshot × 30 Hz ≈ 6 GB/s decode, 32
  çekirdeğe yayılmış) — fan-out kanalları dolar, oda drop sayar. Yani
  ölçülen duvar "oda adımları bütçeyi aşıyor" değil: **p50 bütçesi 10k'ya
  kadar korunan, kuyruk (p99/max) 6k+ da aşan, ve 10k'da in-proc istemci
  katmanının doyuğuyla karışan** bir rejim. Temiz oda-CPU duvarını
  ayırmak için ayrı prosesli istemci gerekir (P2 — bu turda yapılmadı;
  in-proc sayısı muhafazakâr alt sınırdır: istemci CPU'su sunucu adımıyla
  yarışıyor). (loadavg monitörünün 40×3 s penceresi 10 000 koşusunun
  başında bitti — 10 000 için 1 dk loadavg örneği yok; yukarıdaki 9-11
  değeri 2k-8k penceresine aittir.)

- [x] **D2 — Üç strateji, aynı yük (N=1 000, 15 s, stagger 1 ms,
  move 150 ms, MTU 1400 B)** — aynı `gsb-loadgen`, aynı hareket profili,
  aynı makine (profil: yarıçap-40 **hedef** halkası, 4 rad/sn → hedef 160
  u/sn; entity 10 u/sn → entity'ler hedefin **peşinde merkez bandında**
  kümelenir; ölçülen ayak izi: PVS'te sadece A+B sektörleri dolu, c5'te
  ~16 hücre, max hücre 118 üye). Bu geometri yorum için kritik:

  | strateji | out/conn (bps) | sunucu out | adım p50 (µs) | max (µs) | bütçe aşımı | tepe payload (B) | aşım | rec/tick | overlap_x |
  |----------|----------------|-----------|---------------|----------|-------------|------------------|------|----------|-----------|
  | `all` | 247 813 | 247,8 Mbit/s | 3 126 | 8 165 | %0 | 8 809 | 445 | 1 000,0 | 1,00 |
  | `spatial` c20 | 180 493 | 180,5 Mbit/s | 3 126 | 7 962 | %0 | 8 678 | 4 521 | 5 496,0 | 5,50 |
  | `spatial` c5 | 44 845 | 44,8 Mbit/s | 3 126 | 9 392 | %0 | 5 993 | 5 118 | 8 282,3 | 8,28 |
  | `team` | 247 825 | 247,8 Mbit/s | 1 563 | 4 368 | %0 | 8 819 | 890 | 2 000,0 | 2,00 |
  | `pvs` | 126 002 | 126,0 Mbit/s | 1 563 | 4 481 | %0 | 5 694 | 1 333 | 1 367,5 | 1,37 |

  **Yorum (ölçüm, varsayım değil):**
  1. **Bant (stratejinin asli amacı):** c5 `all`'e göre **%82** az
     (247,8→44,8 kbps/conn) — önceki turun c5 @1000 sayısıyla (39,7 M)
     tutarlı; PVS **%49** az (126,0 kbps/conn); c20 %27 az.
  2. **Team, bu geometride sıfır bant kazancı:** 25 birimlik menzil +
     merkez bandı kümelenmesi → her iki takım da haritanın tümünü görüyor
     (rec/tick=2 000 = tam dünya × 2; out/conn `all` ile birebir aynı
     247,8 kbps). Bu bir strateji zayıflığı değil, **yük profili özelliği**:
     gerçek MOBA arenasında (geniş harita, seyrek görevli) aynı kod
     düşmanların çoğunu gizler; burada 1 000 entity'nin ~%50'si her
     takımdan her görevli menzilinde. Ölçüm dürüstçe bunu gösterir: team
     sisli, yoğun merkezî kümelenmede bedelini (2× kodlama) öder,
     kazancı sıfırdır.
  3. **PVS, bu geometride %49 bant + 1,37× kodlama:** kümelenme güney
     bandında (A/B) olduğundan her oyuncu dünyanın ~yarısını görüyor
     (A→A∪C, B→B∪D); kuzey sektörleri (C/D) boş. Kuzeyi dolduran bir yük
     PVS kazancını değiştirir (tablo statik; davranış kodda değil ölçümde
     değişir).
  4. **Adım süresi:** 1 000'de beş strateji de bütçenin %5'inde (p50
     1,5-3 ms; histogram kuantumlu — 1 563/3 126 µs iki komşu kutu);
     strateji seçimi bu ölçekte CPU'da görünmez, bantta görünür. CPU
     farkı D1'de (N ölçeğinde) beliriyor.

- [x] **D3 — Overlap ölçümü + "birim başına tek kodlama" kararı:
  UYGULANMADI (ölçüm + eşik ile gerekçeli)** — `overlap_x` =
  tick başına kodlanan kayıt / üye sayısı (`RoomLogic::encoded_records`
  ile ölçüldü, bkz. C maddesi): `all` 1,00 · PVS 1,37 · team 2,00 ·
  c20 5,50 · c5 8,28. Önerilen tasarım: atomik parça (hücre/sektör) başına
  **bir kez** kodla, istemci gerekli parçaların frame'lerine
  "abone" olsun (batch içi k küçük frame; boş parça da paketsiz kalamaz —
  self-contained union'un eksiksizliği için). Takas: kodlama
  overlap_x → 1,0'a iner; karşılığında istemci tick başına **k frame**
  taşır (spatial c5: k=9 hücre; PVS: k=2-3; team: k≈N/2 — pratik
  sonsuz). **Eşik:** tasarımı uygulamak, kodlama tasarrufunun istemci
  başına k ek frame'in maliyetini aştığı durumda anlamlı:
  `overlap_x − 1 ≥ k · (c_frame/c_enc)`. Ölçülenler: (a) N=1 000'de
  kodlama hacminin 1 000→8 282 kayıt/tick (8,3×) aralığındaki tüm farkın
  adım ortalamasına etkisi **~%11** (1 891→2 104 µs) → bu ölçekte
  marjinal kodlanan kayda düşen pay ≪ 1 frame fan-out payı (fan-out,
  D1'de 8-10k'ta drop üretici olan yol); (b) D1: doyan eksen **istemci
  başına iş** (drop + late tick), kodlama µs'leri değil. Eşik sayılarla:
  c5 için `overlap_x ≥ 1 + 9·(c_frame/c_enc)`; ölçüm
  `c_frame ≥ c_enc`'e uyumlu (fan-out yolu doyan yol) → eşik ≈ 10;
  ölçülen 8,28 **eşiğin altında**. PVS (k=2-3): eşik ≈ 3-4, ölçülen 1,37 —
  çok altında. Team (k≈N/2): eşik ≈ 500, ölçülen 2,0 — asla. **Karar:
  uygulanmadı.** Mevcut mimari zaten "grup başına bir kez kodla, `Arc`
  ref ile paylaş" — kalan overlap, görünürlük öneliminin doğrudan
  sonucudur (bir entity birden çok gruba görünür); onu sıfırlamak parça
  tanımıni atomiğe (hücre) indirir ve bedelini istemci başına frame sayısına
  yazar. Ölçek büyüdükçe (D1: 10k) bu takas daha da kötüleşir: kodlama
  tasarrufu ∝N iken ek frame maliyeti ∝k·N. Tekrar değerlendirme koşulu
  (eşik): `overlap_x` ölçümü 10'u (spatial k=9, c_frame≈c_enc) aşarsa —
  örn. 3×3 yerine 5×5 blok, ya da 9×'ten çok komşuluk taşıyan bir görünürlük
  kuralı seçilirse.

- **Bu turda bilinçli olarak yapılmayanlar:** (1) Ayrı prosesli
  istemcilerle D1 (oda-CPU duvarını in-proc decode'dan ayırmak — P2'ye
  kalır; in-proc sayılar muhafazakâr alt sınırdır ve bu turda yeterli
  soruyu — "p50 bütçe 10k'a kadar dayanır mı, kuyruk nerede aşılır" —
  yanıtladı); (2) loadgen hareket profilinin "entity'ler hedef halkasında"
  olması (hedef 160 u/sn, entity 10 u/sn → merkez bandı kümelenmesi;
  önceki turun sayılarıyla süreklilik için profil değiştirilmedi —
  yorumlaması her tabloda belirtildi); (3) PVS haritasının
  parametrize edilmesi (elle 4 sektör bu turun arenası; gerçek harita
  yazarlığı oyunun malı); (4) team stratejisinde ward/`VisionSource`
  componenti (A2 maddesi, spec gerginliği — "aynı component seti"
  çerçevesi eşit-yarıçaplı oyuncu kaynaklarıyla sınırlandı); (5)
  `encoded_records`'ün delta yayınına (P2) genişletilmesi.

- **Spec gerginliği (rapor):** A2'de "görevli kaynak = unit **ve/veya
  ward**" ifadesi, turun "aynı component seti" çerçevesiyle çelişir —
  ward, `VisionSource` gibi **yeni** bir component ister. Tur çerçevesine
  sadık kalındı: her oyuncu entity'si eşit yarıçaplı kaynak; orphan
  entity'ler her takıma yayılır. Ward desteği, sözleşmeyi (kaynak
  menzilinde görünür) bozmaksızın tek component eklemeyle geri
  getirilebilir.

Ham çıktı: `target/loadout/*.txt` (gitignore; commit mesajında RESULT
satırları). Makine: AMD Ryzen 9 7950X 16C/32T, 124 GB RAM, rustc 1.95.0,
paylaşımlı masaüstü (arka plan ~2-3 çekirdek).

## Kapatılanlar (ayrı proses + yayılma profili + takım-state turu)

Bu tur, önceki turun açık bıraktığı üç soruyu kapatır: (1) in-proc yük
sayılarının duvarı sunucu CPU'su muydu, (2) takım sisli kümeli geometride
sıfır kazanç veriyordu — seyrek/gerçek MOBA geometrisinde ne verir,
(3) tasarımın gizli "grup = konum" varsayımı var mı (takım, conn paritesinden
türetildiği için `group_of` world okumuyordu — kanıtı kolaylaştırmıştık,
bu tur kolaylık kaldırıldı). E1/E2 iki küçük hata düzeltmesi. Test: 58 →
**60** (+1 runtime takım-değişim + 1 ayrı-proces duman testi; hiçbir eski
test değiştirilmedi/silinecek/`#[ignore]` yapılmadı).

- [x] **E1 — `gsb-loadgen --help` panik atıyordu** — `-h/--help` artık
  kullanım metni basıp `exit(0)`; bilinmeyen flag hâlâ panic (sessiz
  yok sayma bug'ını maskelerdi) ama mesajda `(try --help)` işareti var.
  Elenen: (a) **clap** — ~150 satırlık tek amaçlı tool için bağımlılık ağırlığı;
  elle parser zaten 15+ flag'i taşıyor, üç modu da aynı; (b) **bilinmeyeni
  sessizce yok say** — E1'in kendisi o sınıfın habercisiydi.

- [x] **E2 — "server room (final)" `hz=0.00` vs RESULT `server_hz=30.00`** —
  kök neden spec'in tahminine yakın ama farklı: son raporun "önceki aralık
  yok" değil, **0 örnekli pencere** olması — oda örnekleme temposu (1/sn,
  A2: tick başına değil, rapor periyodu başına tek örnek) rapor temposuyla
  faz-kilitli değil; kapanışta son rapor 0-örnekli bir pencere kapatabilir
  (Δsteps=0 → hz=0.0). Düzeltme: final satırı tüm koşu ortancasını (RESULT'un
  `server_hz`'iyle aynı sayı) basıyor; `RoomReport::hz` dokümanına not:
  0 örnekli pencerede hz=0.0 "bu pencerede örnek yok" demektir, tüketici
  pozitif pencerelerin ortancasını almalı. Elenen: (a) son raporu önceki
  pencereden doldur — gerçekliği çarpıtırdı (pencere var, örneği yok);
  (b) örnek+rapor temposunu faz-kitle — metrik semantiği (örnek-aralığı
  hızı, A2) bilerek böyle; görüntü sorunu için core değişikliği.

- [x] **A — Ayrı prosesli istemci modu** — in-proc modun duvarı istemci
  decode'ıydı (10k'da ~6 GB/s protobuf decode, sunucuyla çekirdek
  paylaşımı); sunucu adımlarının gerçek duvarını ölçmek için sunucu artık
  **ayrı proses**te koşabiliyor. Çözüm (tek binary, üç mod):
  - `--serve`: sunucu process'i (in-proc'un aynısı, `Config`'ten).
    `--metrics-listen HOST:PORT` verilirse collector'ın **kanal sink'i**
    (in-proc'un aldığı `MetricReport` struct'larının ta kendisi) ikili bir
    TCP akışına basılır: `[u32 magic][u32 len][LE body]`, 1 Hz. Sunucu
    metrik toplama yolu **aynen korunur** — stdout parsing'i yok, log
    formatı API değil. Flag verilmezse `gsb-metric` log'a yazar
    (`gsb-server` davranışı).
  - `--orchestrate N --procs P`: orkestratör sunucuyu (`--serve`) + P
    istemci process'ini (N istemci, `--offset` ile ardışık küresel id;
    stagger ve profilin id-determinizmi tüm koşu boyunca tutarlı)
    doğurur; istemci çocuklarının `CLIENT` satırları + metrik soketini
    tek RESULT'a birleştirir.
  - **Pinning (`--pin`)**: `unsafe` yasak (pre_exec/sistem çağrısı yok)
    → affinity `taskset -c` sarmalayışıyla; /sys topology'den **SMT-farkında**:
    sunucu ilk 8 fiziksel çekirdeği (16 mantıksal), istemciler kalanı
    round-robin. `taskset` yoksa uyarı + pinsiz koşu (izolasyon o zaman
    sadece proses ayrımına dayanır).
  - **CPU muhasebesi**: `/proc/<pid>/stat` (utime+stime, USER_HZ=100),
    250 ms'lik bekleme turlarında örnekleme — `/proc` reap'te yok olduğundan
    t1 = çıkıştan önceki **son canlı** okuma. RESULT'a: `server_cpu_s` /
    `clients_cpu_s` + `affinity` (çekirdek kümeleri) + `server_pid` /
    `client_pids`.
  - **Neyi feda etti:** (1) istemci varış anları bu process'te yok →
    çocuklar ham değerleri (`connect_ms`, önceden hesaplanmış `hz`)
    `CLIENT` satırında basıyor, orkestratör **ham değerleri** birleştiriyor
    (toplamlar + ham değerler üzerinde percentile → tam, "özetlerin
    özeti" değil); (2) orkestratör iki tarafın da ebe'si (sunucuyu da
    o doğuruyor) → "dış sunucu + bu istemciler" senaryosu `--addr` modunda
    kalıyor; (3) pin için `taskset` binary'si şart (yoksa pinsiz koşu);
    (4) metrik raporları için bir loopback ek istasyonu (1 Hz, önemsiz).
  - Elenen: (a) **thread'ler (ayrı tokio runtime'ları)** — CPU izolasyonu
    yok (amaca aykırı); (b) **ayrı istemci binary/crate'i** — aynı
    binary = metrik codec'inin iki ucu aynı derlemede (format sürüklenemez)
    + istemci kodu zaten `run_client`'ta; (c) **`gsb-metric` log satırlarını
    parse et** — spec yasakladı, ayrıca log formatı API değil (tracing
    filtresi değişse parser kırılır).

- [x] **B — İkinci yük profili: `--profile spread`** — eski profil
  (yarıçap-40 hedef halkası, 4 rad/sn) entity'leri merkez bandında
  kümelendiriyordu → görünürlük stratejileri ayrıştırılamıyordu. Yeni
  profil: her istemcinin deterministik bir **evin** var (±`spawn-half`
  haritada uniform ızgara; sunucu spawn'ı aynı dağılımla — `spawn_half_size`
  config'i, vars. 50 → eski arena bit-bit aynı; spread koşuları 1000 =
  2000×2000 geniş harita), evinin çevresinde yarıçap-20, 0.4 rad/sn
  (hedef 8 u/sn < entity 10 u/sn → entity hedefi takip eder, ayak izi ≈ ev).
  Spawn da aynı dağılımla olduğu koşul **tick 1'den istatistiksel durağan**
  (göç transiyantı yok). Profil RESULT'ta (`profile=ring|spread`) ve
  machine satırında. Eski profil **aynen** varsayılan (tüm önceki ölçüm
  tabanı `ring`; süreklilik bozulmadı).
  - **Dağılım gerekçesi:** uniform, tek değişkenli ve tek doğru seçenek:
    C2 iddiası "geniş harita, seyrek görevli" ve uniform'da görüş kapsamı
    yalnız (kaynak, yarıçap, alan) fonksiyonu (`c = 1−exp(−kπr²/A)`) —
    yorumlanabilir. Herhangi bir kümelenme (merkezli gauss, ikinci halka)
    eski profilin gizlediği kümelenme artefaktını geri getirirdi.
  - Elenen: (a) **merkezli gauss** — yine merkez yoğunluğu; "seyrek" rejimi
    sadece kuyruklarda; kapsam hesabı 2B integral; (b) **evsiz rastgele
    uniform hareket** — 10 u/sn × 30 sn = 300 birim difüzyon → geometri
    bulanıklaşır, koşul durağan değildir; (c) **jittersız ızgara** — tüm
    entity'ler 20×20 noktasında: komşuların payload'u byte-identik olur,
    `overlap_x` artefaktı.

- [x] **C1 — D1 break-even, ayrı prosesle ölçüldü** — spatial c5, ring
  profili (D1 sürekliliği), 30 sn, stagger 0,5 ms, pin (sunucu 8 fiziksel
  çekirdek). `server_cpu_s`/`clients_cpu_s` = 30 sn'de tüketilen çekirdek
  saniyesi (sunucu havuzu 16 mantıksal × 30 = 480 çekirdek-sn):

  | N | adım p50 | adım max | bütçe aşımı | server_hz | drop | late max | server_cpu_s (%havuz) | clients_cpu_s | out/conn |
  |---|----------|----------|-------------|-----------|------|----------|----------------------|---------------|----------|
  | 5 000 | 12,5 ms | 42,9 ms | %1,0 | 30,00 | 9 086 | 16 ms | 89,7 (%19) | 279,0 | 490,8 kbps |
  | 8 000 | 25 ms | 92,3 ms | %36,6 | 29,88 | 45 596 | 1,80 s | 120,8 (%25) | 319,7 | 359,3 kbps |
  | 9 000 | 25 ms | 103,6 ms | %44,6 | 28,19 | 30 869 | 2,34 s | 120,3 (%25) | 323,5 | 322,0 kbps |
  | 10 000 | 50 ms | 100,4 ms | %54,8 | 23,21 | 52 771 | 2,17 s | 119,4 (%25) | 298,8 | 269,6 kbps |

  **Cevap:** p50, 33,3 ms bütçeyi **9k ile 10k arasında** aşıyor (9k'da
  25 ms = bütçenin %75'i, hâlâ altında; 10k'da ≥50 ms = %150+, adımların
  %54,8'i bütçeli üstte, `server_hz` 23,2 < 30 → oda actor'ü ticker'dan
  geride, 252 late tick). **Sunucu CPU'su izole edildi ve doymadı:**
  ayrık çekirdek kümleri (`affinity=`), istemci decode'ı ayrı havuzda
  (`clients_cpu_s` ~280-320), sunucu havuzu 10k'da bile **%25** dolu
  (119/480 çekirdek-sn) — doyan parça **tek room actor'ünün serisel adım
  yolu** (read→convert→systems→encode→fan-out tek task'ta; 12 boş çekirdek
  varken adım 33,3 ms'e sığmıyor). Mimari çıkarım: bir sonraki kaldıraç
  oda paralelliği/segmentasyonu (P2), ek çekirdek değil. D1'in in-proc
  10k sayısı (p50 12,5 ms, %18,9) ile karşılaştırma: in-proc, p50 duvarını
  *maskeleyip* kuyruğu istemci decode gürültüsüyle bozuyordu (bimodal +
  1,34 s late) — duvar aynı odadaydı, ölçüm istemci CPU'su ile karışıktı.
  (Makine koşu sırasında paylaşımlı: loadavg ~10/32; pin, sunucu kümesini
  istemcilerden, ama arka plan kullanıcılarından korumaz — iki koşul grubu
  aynı makine hâlinde ölçüldü, karşılaştırma koşullar arası değil koşu
  içi.)

- [x] **C2 — Takım sisli uçtan uca (N=1 000, 20 sn, pin)** — aynı kod,
  üç geometri:

  | koşu | profil | out/conn (bps) | rec/tick | overlap_x | adım p50 | paket içeriği |
  |------|--------|----------------|----------|-----------|----------|----------------|
  | `all` | ring | 246 991 | 1 000,0 | 1,00 | 782 µs | 1 000/1 000 |
  | `team` | ring | 246 885 | 2 000,0 | 2,00 | 1 563 µs | 500 kendi + 500 düşman (kümüli: hiçbiri gizlenmez) |
  | `team` | **spread** | **177 526** | 1 153,2 | 1,15 | 1 563 µs | 500 kendi + ~77 düşman |

  `all` sayısı D2 sürekliliğini doğruluyor (D2: 247 813; Δ%0,3). Kümüli
  geometride `team` yine **sıfır kazanç** (D2 yeniden üretimi: 246 885 ≈
  246 991) — beklendiği gibi, strateji zayıflığı değil geometri
  özelliği. **Geniş haritada iddia doğrulandı:** "gerçek MOBA arenasında
  (geniş harita, seyrek görevli) aynı kod düşmanların çoğunu gizler" →
  ölçülen: takım paketinde 500 kendi + **~77 düşman** (1 153,2/2 − 500) →
  **düşman takımın ~%84,6'sı her an pakette yok**. Kazanç, kayıt sayısı
  (gerçek gizlilik etkisi) olarak **%42,3** (2 000→1 153 rec/tick;
  `all`'in %57,7'si görünüyor); bayt olarak **%28,1** (177,5 vs 247,0
  kbps) — fark, geniş haritanın kendisinden: |koordinat|≤1 000 → sint32
  zigzag 2 bayt (ring'de 1) → kayıt başına ~%25 daha büyük frame; gizlilik
  oranı kayıt sayısında, bant oranı koordinat kodlamasıyla seyreltiliyor.
  Not: 1 000 oyuncuda paket (≈7 KB) 1 400 B MTU'yu yine aşıyor
  (`snap_overflows` 1 186) — gerçek MOBA oyuncu sayısı (10-30) için
  aşılmaz; MTU sorunu ölçek sorunu, strateji sorunu değil.

- [x] **D — Takım, world state olarak (ve "grup = konum?" sorusunun
  yazılı cevabı)** — spec'in doğruluğu: önceki tasarımda takım `conn.id`
  paritesinden türetiliyordu, `group_of` **world okumuyordu** — gruplamanın
  mekansal olmadığına en kolay kanıttı (ve kolaylaştırdığımız buydu). Bu
  tur: takım üyeliği **dünya içi oyun durumu** oldu.
  - `TeamMember(Team)` componenti (join'de `on_join` yazar; kural — conn
    paritesi — `team_of`'da, artık "join anı kuralı" olarak dokümante);
    `group_of` conn → entity → **world'deki `TeamMember`** okur; AOI'nin
    `Position` okumasıyla birebir aynı şekil. `rebuild` sorgusu
    `(&WireId, &Position, Option<&TeamMember>)` (eski `ent_team` HashMap'i
    kaldırıldı).
  - **Runtime takım değişimi** testte kilitli: entity'nin `TeamMember`'i
    yazıldığında bir sonraki tick'te group takibe alır — entity 0. takımın
    paketinden düşer, 1. takımın paketinde **aynı wire id** ile belirir
    (pozisyonlar öyle seçildi ki iki takımın paketi de gözle görülür
    değişir; snapshots kayıt taşır, rol taşımaz → byte-identik paket
    ince ayrıntısı test dokümanında). **Kimlik değişmezliği korundu**:
    group geçişi ≠ wire kimlik geçişi.
  - **Aradık, yok:** Tasarımda gizli "grup = konum" varsayımı **kalmadı**.
    Dört stratejinin tümü aynı biçimde: "grup = f(world, conn)". Konumdan
    gelen anahtar (AOI/PVS) ile oyun durumundan gelen anahtar (takım)
    core'un grup mekaniklerinde (tick başına yeniden değerlendirme,
    grup başına snapshot/ledger, grup geçişinde kimlik sürekliliği)
    **birebir aynı yoldan** geçer; core'un ikisini ayırt eden tek bir
    dalı/özel durumu yoktur. Takım, "dünyada yaşayan oyun durumu"dur —
    mekansal değil, ama world dışında da değil.
  - **`VisionSource` eklenmedi (gerekçe):** Bugün onu `TeamMember`'den
    ayırt eden *okuyucu yok* — tüm görüş kaynakları, eşit config
    yarıçapıyla, tam da `TeamMember` taşıyan entity'ler. Eklemek,
    okuyucusuz ölü state olurdu (denetim turunda `Owner`'u kaldıran
    disiplinin aynısı). Seam hazır: ward/trap özelliği geldiğinde
    `rebuild`'in kaynak iterasyonu tek değişim noktası; sözleşme
    ("kaynak menzilinde görünür") değişmez.

Ham çıktı: `.scratch/*.txt` (gitignore; commit mesajında RESULT satırları).
Makine: AMD Ryzen 9 7950X 16C/32T, 124 GB RAM, rustc 1.95.0, taskset
2.42.2, paylaşımlı masaüstü (koşu sırasında loadavg ~10/32 — C1 koşuları
pin ile, C2 koşuları 8 fiziksel çekirdek sunucu havuzunda).

## Kapatılanlar (koruma katmanı turu)

Bu tur, spec'in garantisini kurar: **sunucu, istemcilerinin yaptığı
hiçbir şeyden dolayı çökmemeli veya kaynak sızdırmamalı.**
Üç hat + doküman (DESIGN §14): (1) oturuş yaşam döngüsü
(yarım açık TCP), (2) kapasite (oda + sunucu geneli), (3) girdi
adaleti (flooding atfesi), (4) kapsam dokümanı. Test: 60 →
**72** (+12: 3 gsb-net + 4 gsb-core + 5 gsb-server e2e; hiçbir eski
test silinmedi/ihmal edilmedi — yeni `Result` join yanıtı
tipine uyan join yardımcılarının imzası güncellendi,
mantiğ/sağlama korunmuş). Makine: 32 core / 124 GB /
rustc 1.95.0 (koşular release).

- [x] **MADDE 1 — Oturuş yaşam döngüsü: yarım açık TCP
  tespit edilip kapatılır (konfigure edilebilir timeout)**
  — Sorun: kablo çekilen/gücü kesilen istemci (FIN/RST gelmez)
  reader pump'ta sonsuza kadar beklerdi; her böyle oturum 3 görev +
  2 kanal + 1 registry kaydı pinlerdi (100k hedefinde sızma).
  **Saati kim tutacak?** Saat, **reader pump'un read deadline'
  ındadır**: her `stream.next()` `tokio::time::timeout(t, …)`
  ile sarılır; **her** istemci frame'i (HEARTBEAT dahil) pencereyi
  sıfırlar; zaman aşımında pump `ConnIn::ServerClosed {
  reason }` yollar, connection actor ERROR 9 gönderip cleanup
  kaskadından çıkar. Bu, bağlantı yolundaki **tek** saattir:
  connection actor'ün tek await'i inbox `recv`'de kalır
  (spec'in sert şartı — `select!` yok), yeni görev / registry
  mesajı / ticker aboneliği eklenmez. Yarım açık TCP "socket
  durumu" ile değil, "T süre boyunca frame yok" ile yakalanır.
  Konfig: `idle_timeout_secs` (vars. 30; 0 = kapalı).
  - **100k maliyeti (seçilen):** 0 ekstra görev; ~15 MB
    (100k bekleyen `Sleep` ≈ 150 B); ~1–2 core (yalnızca
    gerçekten boşta olan bağlantılar uyanır; uyanan çıkar).
  - **Elenen alternatifler (100k matematiği):**
    - **(A) Bağlantı başına timer görevi** (watchdog/interval):
      +100k görev ≈ 100–200 MB (tokio görevi ≈ 1–2 KB);
      her istemci frame'inde yeniden arm → 100k bağlantı ×
      30–100 frame/sn ≈ 3–10M timer işlemi/sn ≈ 5–10% core +
      100k+ allokasyon/sn.
    - **(B) Registry'de son-görülme damgası:** her heartbeat →
      registry mesajı = 100k msg/sn server geneli; registry'nin saat
      tutması gerekir (ticker aboneliği) = kontrol düzleminde
      ikinci beklenen kaynak; 100k msg/sn registry, join yolunu aç bırakır.
    - **(C) Bağlantı başına ticker alıcısı:** 100k
      broadcast receiver; her tick 30/sn × 100k slot yazımı = 3M
      slot yazımı/sn ≈ 3–6% core + 100k receiver durumu;
      **ve** connection actor iki kaynaktan beklerdi (inbox + ticker)
      = `select!` — spec ihlali (karar verici neden).
  - Test: `idle_peer_gets_server_closed`,
    `active_peer_resets_the_idle_window` (tempolu poke — kontrol
    edilebilir `mpsc`-arkalılı `Stream` adaptörü; TCP buffer'ı
    tempoyu yuttuğu için), `eof_reports_peer_closed_not_idle`;
    e2e: `idle_connection_is_closed_by_the_server` (ERROR 9, EOF'tan
    önce), `active_heartbeat_survives_the_idle_window` (250 ms
    heartbeat, 1 s pencere, 3+ ACK).

- [x] **MADDE 2 — Kapasite: oda `max_players` + sunucu geneli
  `max_connections` (ölçüme dayalı varsayılanlar)**
  — Sorun: `CoreError::RoomFull` vardı ama öçülmeyen bir
  dallıydı; oda sınırsız büyürdü, sunucu geneli cap
  yoktu. **Varsayılanlar ölçülen sayıya dayandı** (C1:
  ayrı proses, spatial AOI, 30 Hz, tek oda):

  | Oyuncu | p50 (ms) | server_hz | fan-out drop | late (s) |
  |---|---|---|---|---|
  | 5k | 12.5 | ~30 | 9 086 | ~0 |
  | 8k | 25 | ~30 | 45 596 | ~0 |
  | 9k | 25 | ~30 | 30 869 | ~0 |
  | 10k | **50** | **23.2** | 52 771 | **2.17** |

  p50, 33.3 ms bütçesini **9k–10k arasında** aşıyor
  (not: "drop" sütunu fan-out `dropped_frames`'tir — aksiyon
  düşmü değil). → `max_players` = `Some(10_000)` (ölçülen
  duvar), `max_connections` = `Some(100_000)` (DESIGN §1 hedefinin
  guardrail'i).
  - **Reddi semantiği (karar: nazik Error, sessiz kapatma değil):**
    maliyetler farklı. Oda dolu → **ERROR 8, bağlantı yaşar**:
    reddedilen istemci başka odaya join edebilir/bekleyebilir; sessiz
    kapatma = reddedilen istemcilerin reconnect/backoff fırtınası
    (tam istenmeyen DoS amplifikatörü); maliyet = 1 ekstra frame.
    Bağlantı cap'i → **ERROR 9 + hemen kapatma**: reddedilen
    bağlantının yapabileceği hiçbir şey yok — açık tutmak
    sadece oturum pinler; kapatma EOF'tan önce bildirilir (istemci
    ağ hatası olmadığını bilir).
  - **Spec notu (yer çevikliği):** cap, accept loop'ta değil
    **registry'de** uygulanır — sayım, guardrail'in olduğu yerde
    yaşamalıdır; accept loop, ikinci bir beklenen kaynak eklemeden
    kopmalara gözlemleyemez. Reddedilen bağlantı tabloya kaydedilmez;
    JOIN’in `ServerClosed` ile yaraşı `ConnClosed`'da
    işlenir (dispatcher slot'u drenaj edilir).
  - **Kanıt (ham):** 150 istemci, `--max-players 100`, 10 s:
    `RESULT mode=in-proc visibility=all max_snap_bytes=1400
    clients=150 connected=150 joined=100 left=100 … steps=300
    server_hz=30.00 step_p50_us=130 … dropped=0 … peak_conns=150
    … join_rejected=50 cap_rejected=0 actions_dropped=0
    actions_dropped_top=` (— tam 50 reddi, tam 100 join; reddedilen
    istemcilerin sonraki MOVE_TO'larına sunucu ERROR 6
    (NotInRoom) ile karşılık verir — bağlantı sağlıklı kalır).
    100 istemci, `--max-connections 50`, 10 s: `RESULT mode=in-proc
    … clients=100 connected=100 joined=50 left=50 errors=0 …
    server_hz=29.99 … dropped=0 … peak_conns=50 …
    join_rejected=0 cap_rejected=50 actions_dropped=0 actions_dropped_top=`
    (— registry tablosu asla 50'yi aşmaz; TCP katmanı tümünü kabul eder;
    cap oturum seviyesindedir).
  - Test: `join_rejected_when_room_is_full` (reply `CoreError::RoomFull`,
    kalan üyenin girdisi etkilenmez), `spawn_rejected_when_room_is_full`,
    `conn_opened_rejected_at_connection_capacity` (`ServerClosed` içerik
    "capacity"; `ConnClosed` temiz no-op), e2e
    `room_full_returns_gentle_error_code_8` (B hata alır, yaşar,
    heartbeat ACK'lanır), `connection_capacity_rejects_with_code_9`
    (B: ERROR 9 + EOF; A etkilenmez).

- [x] **MADDE 3 — Adalet: flooding bağlantı başkasının
  aksiyonunu evicted edemez; düşen aksiyon atfeli** — Sorun: eski
  READ, tüm bağlantıların aksiyonlarını tek `Vec`'te merge
  ederdi; `max_pending_actions` aşımında `drain(..over)` = **en
  eski** atılırdı: tek bir flooder'ın backlog'u başkasının
  aksiyonlarını atıyordu (hangi bloğun atılacağı
  `HashMap` sırasına bağlıydı — kurban bile keyfiydi) ve
  söz konusu sayaç oda geneliydi (`dropped_actions`), atfe yoktu.
  - **Çözüm: READ, merge değil sınırlı çekmedir**
    (bounded pull): bağlantı başına tick bütçesi
    `max_actions_per_conn_per_tick` (vars. 16 = 30 Hz'de 480 aksiyon/sn
    ≈ ölçülen 6.7/sn/istemcinin 70×'i) + oda çekme
    bütçesi `max_pending_actions` (vars. 65536). Oda **çektiği
    aksiyonu asla atmaz** (`dropped_actions` yapısal 0; alan format
    uyumu için korunur). Tek kayıp noktası = göndericinin **kendi**
    `Action` kanalı doluyken `try_send` Full (cap 256) — connection
    actor bunu **kendi** metrik örneğinde sayar
    (`m_actions_dropped`), raporda `actions_dropped` (net, kümülatif)
    + `actions_dropped_top` (en çok düşürmüş 5 bağlantı,
    `c{id}:sayı`). Hata frame'i yok, koparma yok (koparmak
    reconnect amplifikatörü olurdu). Hasar sınırı: tek
    saldırgan → odaya en çok 16/tick × 30 = 480 aksiyon/sn +
    256'lik kendi buffer'ı.
  - **Kanıt (ham):** 20 istemci, `--flood-id 7`, 15 s (istemci 7 =
    `c8`): `server net (final): bytes_in=6324 KB …
    frames_in=3234645 … actions_dropped=3225454` /
    `server net (final): actions_dropped_top=c8:3225454` /
    `RESULT mode=in-proc … clients=20 connected=20 joined=20 …
    steps=450 server_hz=30.00 step_p50_us=130 step_max_us=114
    step_over_budget_pct=0.0 dropped=0 late_max_us=668 …
    peak_conns=20 … join_rejected=0 cap_rejected=0
    actions_dropped=3225454 actions_dropped_top=c8:3225454`
    (— 3.2M düşen aksiyonun **%100'ü saldırgana atfeli**;
    diğer 19 istemci temiz 30 Hz: bütçe aşımı %0, fan-out
    drop 0, en geç tick gecikmesi 668 µs).
  - Test: `flooder_cannot_evict_other_connections_actions` (kurbanın
    20/20 aksiyonu içerildi — sıra HashMap'in, atfe garantilenir;
    backlog probe'u: tam olarak 8/tick × 21 tick kadar slot açılır
    = kalan backlog flood'un kendi kanalında), e2e
    `flooder_drops_are_attributed_to_the_flooder` (raporda top[0] =
    flooder ve net toplamla eşitleşir).

- [x] **MADDE 4 — Doküman: DESIGN §14 "Bu base neyi
  hedefliyor, neyi hedeflemiyor"** — Grup değişim birimidir
  (fan-out grup başına, bağlantı başına değil; oda
  tarafında bağlantı başına durum yok; connection actor
  kasıtlı ince durum makinesi). seq/ack yok (teslim garantisi
  TCP'nin; snapshot tam durum; kayıp → bir kademe bayatlık, keepalive
  sınırlar; seq/ack = bağlantı başına durum = başka bir
  mimari — kapı rUDP yolunda, §6). Ölçülen tavan **tek
  odadadır** (C1 duvarı 9–10k; oda actor'ün tek iş parçacıklı
  olmasının doğal sonucudur) → ölçekleme
  kaldıracı **oda segmentasyonu** (`max_players` = ölçülen
  duvar). Sığan aileler: battle royale, arena/MOBA, zoneli MMO/AOI,
  instanced dungeon; sığmayanlar: oda sınırı olmayan tek dünya,
  bağlantı başına oturum durumu, lockstep/sıra garantileri.
  Ayrıca: §3'e oturum saati maddesi, §4'e sınırlı çekme
  (bounded pull) tanımı, §5'e ERROR kod tablosu (Unity referansı
  + `base.proto` yorumu), §10 v1 kısıtları tablosu güncellendi,
  §12 metrik tablosuna `actions_dropped`/`actions_dropped_top`,
  `config.example.toml`'a üç yeni anahtar.

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
  AOI önceki turda kapatıldı**; sonraki turda görünürlük **strategi
  seçimi** oldu (`all`/`spatial`/`team`/`pvs`, `Config.visibility`),
  break-even önce in-proc (p50 10k'ya kadar bütçe altında, kuyruk 6k+
  aşıyordu — istemci doyuğu karışıktı) ve bu turda **ayrı prosesle net
  ölçüldü**: p50 bütçeyi **9k-10k arasında** aşıyor (10k: ≥50 ms, %54,8
  adım bütçeli üstte, `server_hz` 23,2); 10k'da sunucu çekirdek havuzu
  **%25** dolu → duvar **tek room actor'ünün serisel adım yolu**.
  `Visibility` trait'i gerekmedi (C maddesi). Kalan: **oda segmentasyonu**
  (10k+ duvarı için — artık net ölçülmüş: ek çekirdek değil, oda
  paralelliği) ve "birim başına tek kodlama" (D3'te eşik altı bulundu;
  overlap_x ölçümü eşiği aşarsa yeniden açılır).
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
