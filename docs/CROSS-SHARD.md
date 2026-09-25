# gsb: Cross-Shard Etkileşim ve Border Paylaşım Tasarımı

> Durum: TASARIM NOTU (dış danışma diyaloğundan derlendi); §2–§4
> UYGULANDI — uzak etki §4b (C1), crystallization §4c (C2). §2–§5 etkileşim
> desenleridir; §6–§8 border paylaşımının delta'ya evrimi ve ölçüm planıdır
> (ölçüm turu yürütülüyor). Uygulama turları bu dokümanı sözleşme alır.

## 1. Bağlam

Sharded odada varlık simülasyonunun otoritesi her zaman sahibi shard'tadır;
komşu shard'lar sınır şeridini her tick tam durum olarak ödünç alır
(BORDER fazı — shard.rs "Boundary visibility"). Bu doküman iki soruyu
kapatır: (1) sınırdan *etkileşim* nasıl taşınır, (2) *ödünç verme*
mekanizmasının kendisi full'dan delta'ya evrilir mi?

## 2. Uzak-etki deseni (sniper / büyü / DoT)

İlke: hedefi görmek ≠ hedefe dokunmak. Saldıran shard, hedefi zaten
**ödünç kayıt** olarak görür; isabet/açı/menzil kontrolünü LOKAL yapar,
etkiyi hedefin otoritesine tek mesajla devreder:

```
ShardMsg::RemoteEffect { target_identity, target_epoch, payload }
```

Mesaj sözleşmesinin zorunlu üç bileşeni:

- **Idempotency:** aynı efekt iki kez uygulanamaz (epoch + efekt-seq);
  RECONNECT'in exactly-once disiplininin aynısı.
- **Kaynak-atfı:** kill kredisi/istatistik için saldıran kimliği payload
  içinde; otorite shard işler.
- **Anti-cheat yerel:** menzil/açı/hız doğrulaması ATICIN shard'ında
  yapılır; otorite payload'a körü körüne güvenmez (ikinci kontrol
  oyun-politikası).

Rota seçimi: hedefin shard'ı bilinmiyorsa broadcast-resume desenindeki
gibi tüm shard'lara gönder + epoch-guard tek-kabul; komşular küçükse
(≤16) bu yeterli.

## 3. Mermi taşıma: iki desen

| Desen | Ne zaman | Püf noktası |
|---|---|---|
| Mermi = entity, Migrate ile | Yavaş mermi, izlenebilir yol | Mevcut protokol aynen çalışır |
| Hitscan = uzak-etki | Anında isabet | §2; tunneling riski yok |

Tunneling notu: çok hızlı mermi bir tick içinde şeridi aşabilir;
Migrate tick-sınırında olduğundan isabet çözümünü §2'ye devretmek
güvenli genel cevaptır.

## 4. Yakın dövüş: dört katman

Melee'nin sniper'dan farkı **sürekli ve çift yönlü** olması. Katmanlar:

1. **Lokal isabet:** saldıran, hedefin ÖDÜNÇ kopyasına göre menzil/açı
   kontrol eder (bayatlık ≤1 tick ≈ 33 ms — melee ölçeğinde kabul;
   istenirse attack-anında güncel borrow istenebilir).
2. **Etki mesajı:** hasar, hedef otoritesine RemoteEffect ile.
   Ping-pong maliyeti önemsizdir (tick başına birkaç yüz bayt).
3. **Deterministik hakem:** A ve B aynı tick'te karşılıklı etki
   uygularsa işlenme sırası `(attacker_id, seq)` kıyasıyla sabittir —
   her shard aynı sonuca varır. Dağıtılmış kilit YASAK (aktör ilkesi).
4. **Crystallization:** sürekli cross-seam dövüş tespit edilirse
   (iki oyuncu K tick'tir sınırdan ayrılmıyor), taraflardan biri
   proaktif olarak karşıya migrate edilir → dövüş tek shard'a
   "kristalleşir" ve problem, zaten çözülmüş migrasyona dönüşür.
   **UYGULANDI (C2, §4c)** — tespit etki akışıyla, taşınan yüksek wire,
   sahiplik dövüş sürerken bölgeden ayrıştırılır; sapmalar §4c'de.

Elenen alternatif: dağıtılmış kilit/joint-authority çözümleri — aktör
modeline aykırı; tasarım seviyesinde de kaçınılır.

## 4b. C1 sonucu — seam ötesi okuma ve uzak-etki (branch `xseam/c1-remote-effect`)

> Buradaki "C1", seam ötesi etkileşim paketinin ilk turudur; §7'deki
> C1/C2 ölçüm koşularıyla ilgisi yoktur.

§2'nin uzak-etki primitifi ve §4'ün 1–3. katmanları uygulandı; 4. katman
(crystallization) sonraki turdur. Kod: `gsb-core/src/shard/{effect,seam}.rs`,
`shard/actor/tick/effects.rs`; kit: `gsb-kit/src/sharded/seam.rs`;
MMO: `gsb-demo-mmo/src/{combat,effect}.rs`.

**Parça 1 — ödünç kayıtlar oynanışa açık.** Sharded tick kancaları
(`ShardLogic::ingest_seam` / `update_seam`, varsayılanları `ingest` /
`update`) bir `CrossSeam<'_, Strip>` alır: aktörün komşu-başına
görünümleri YERİNDE okunur (tick başına kopya yok), karantinadaki görünüm
hariç — yani bu tick'in snapshot'ına katlanan kümenin aynısı. Bayatlık
≤ 1 tick (kayıt, borç verenin önceki — ya da gövdesi önce koştuysa bu —
tick'inin sonundaki durumu). `lent(wire)` kaydı ve BORÇ VERENİ
(`Lent { wire, lender, state }`) döndürür; arama sırası borç veren
indeksine göre artandır (deterministik). Kit'in `Seam`'i bunu odanın
sahip-olunan wire tablosuyla birleştirir: `local(wire)` yerel entity,
`lent`/`lent_iter` yerel olanı ATLAR (sahip kazanır — yeni göç etmiş
entity bir tick boyunca eski shard'ından da ödünç görünür). "p'ye r
mesafedeki her şey" = world sorgusu + `lent_iter().filter(..)`;
birleşik kopya kurulmaz.

**Parça 2 — `RemoteEffect`.** Katman: YENİ bir `ShardMsg` varyantı
(`NeighborMsg` zaten `ShardMsg`'in takma adı), yani Migrate/Border'ın
bindiği aynı `ShardLink` FIFO'su — yeni kanal yok, yeni await yok.

```
ShardMsg::RemoteEffect(RemoteEffect {
    target: u64,            // hedefin wire kimliği
    source: u64,            // kaynak entity'nin wire kimliği (atıf; 0 = yok)
    id: EffectId { origin: usize, epoch: u64, seq: u64 },  // idempotency anahtarı
    at_tick: u64,           // kaynağın tick'i (hizalama + bayatlık damgası)
    hops: u8,               // yönlendirme sayısı
    payload: Bytes,         // oyunun baytları, çekirdek okumaz
})
```

- **Yayım:** `CrossSeam::emit(target, source, payload)` hedefi ÖDÜNÇ
  VEREN komşuya yönlendirir, kimliği çekirdek basar. Ödünç verilmiyorsa
  `NotLent`, tick bütçesi (`EFFECT_BUDGET_PER_TICK` = 128) dolduysa
  `Budget` — reddetme eşzamanlı, arkadan düşürme yok. Kit'in `Seam`'i
  yerel hedefe `Local` der (doğrudan yazılır). Gönderim faz 3b'de
  (sistemlerden sonra).
- **Uygulama:** CONTROL drain'inin SONUNDA (faz 0d — drain'deki bir
  Migrate hedefi önce kurar; girdi ve detach sweep'inden önce, yani veto
  bu tick'in darbesini görür), kaynağın `at_tick`'inden bir sonraki
  tick'te (Migrate'in kurulum kapısının aynı hizalaması: erken gelen
  bekler), `(source, origin, seq)` sırasıyla.
- **Idempotency ve sınırlı dedup:** köken-shard başına sabit boyutlu
  kayan pencere (`EFFECT_WINDOW` = bütçe × (yaş tavanı + 1) = 1024 seq,
  256 B + yüksek-su işareti). Kanıt: köken tick başına ≤ bütçe kadar seq
  basar, `EFFECT_MAX_AGE_TICKS` (7) aşan hiçbir efekt kabul edilmez —
  kabul edilebilir her efektin seq'i penceredeki en yüksekten
  `bütçe × (yaş + 1)` içinde kalır; gerçek bir efekt pencereden asla
  düşmez, durum `shard_count` pencere, trafik ne olursa olsun.
- **Tam kanal politikası: sonraki tick yeniden dene, sınırlı.** Darbe
  oynanıştır (sessizce düşen darbe hatadır): dolu komşu kutusu efekti
  `EFFECT_RETRY_CAP` (1024) kapasiteli tampona alır, damgası değişmeden
  sonraki tick'te önce o gider; yaş tavanında düşer. Taşma ve kapalı
  link düşürür ve sayar. Elenen: düşür+say (tek tick'lik geçici tıkanma
  darbe kaybettirir); sınırsız kuyruk (yasak); bloklayan gönderim
  (tick gövdesinde await yasak).
- **Göç etmiş hedef: YÖNLENDİRME.** Göç eden shard, Migrate başarıyla
  kuyruğa girdiğinde `wire → (yeni sahip, bitiş tick'i)` yazar
  (`EFFECT_FORWARD_TTL_TICKS` = 11); bu wire'a gelen efekt yeni sahibe
  iletilir (`hops` ≤ 3) — kayıt dünyasında BİR tick daha duran "ölümlü
  kopyaya" asla uygulanmaz; entity geri gelirse kayıt silinir. TTL
  sonrası gelen yetim sayılır. Elenen: düşür+say (her göç sınırında
  darbe kaybı); broadcast + epoch guard (aşağıda sapma 3).
- **Atıf:** `source` zarfın içinde; otorite (MMO) kill kredisini ona
  yazar.
- **Anti-cheat yerelliği:** menzil saldıranın shard'ında ödünç kayda
  karşı; MMO otoritesi politika olarak yeniden denetler (bayatlık 3
  tick, saldıranı gördüğü yerden menzil + 2 m pay, hasar tavanı).
- **Sayaçlar:** shard başına kümülatif `EffectStats`, ~1 sn'de bir
  değiştiyse `remote_effect_summary` info satırı (metrik raporuna
  girmedi — `RoomReport`'un fold kuralı gerekmedi).

**§2–§4'e göre sapmalar (gerekçeli):**

1. *`target_epoch`* ayrı bir hedef-epoch'u değil, efekt kimliğindeki
   ODA ENKARNASYONU epoch'udur (registry'nin kurulum nesli). Wire
   kimlikleri bir enkarnasyon boyunca asla yeniden kullanılmaz (aralık
   bölümleme + göçte kimlik korunur), yani hedef kimliği tam olarak
   `(wire, epoch)`; hedef-başına bir epoch hiç bilgi taşımazdı.
2. *Atıf "payload içinde"* değil ZARFTA: çekirdek §4.3 sıralamasını onunla
   yapar, oyunun codec'i olmadan okunabilmeli.
3. *Rota: "shard bilinmiyorsa broadcast + epoch-guard tek-kabul"*
   uygulanmadı. Hedefin shard'ı hep bilinir (borç veren); göç etmişse
   eski sahip iletir (DISTRIBUTED §4 EFFECT'in "forwarding"i). Broadcast
   elendi: link'ler yalnız komşular arasında (komşu olmayan yeni sahibe
   ulaşamaz), 128'lik paylaşımlı kutuya N kat baskı, ve tek-kabul yine de
   "ölümlü kopya" bilgisini (pending-out) gerektirirdi.
4. *§4.3 anahtarı `(attacker_id, seq)`* → `(source, origin, seq)`: seq
   köken-başına olduğu için köken anahtarı tamamlar. Ek olarak BİR TİCK
   HİZALAMA kapısı: shard tick gövdeleri iç içe geçtiğinden, kapı olmadan
   aynı köken tick'inin efektleri planlama şansıyla iki tick'e bölünürdü;
   sıralama ancak kapıyla "her koşuda aynı" olur (sağlıklı zarf içinde;
   bir tick geride kalan shard'da Migrate'in bilinen bozulmasıyla aynı).
5. *DISTRIBUTED §4 "yalnız gördüğünden yeniyi uygula, eskiyi at"*
   (yüksek-su) değil, KAYAN PENCERE: yönlendirilen ya da yeniden denenen
   gerçek bir efekt daha yeni bir efektten SONRA gelebilir; yüksek-su onu
   düşürürdü. Hedef-başına değil köken-başına: durum sabit.
6. *Bayatlık:* K oyunundur (MMO: 3 tick) ve damga taşınır; çekirdek
   ayrıca bir TAŞIMA tavanı uygular (7 tick) — dedup penceresinin sınırı
   ondan türetilir.
7. *Katman 1 "saldırı anında güncel borrow"* uygulanmadı (≤1 tick
   bayatlık kabul; MMO'nun menzil payı bunu karşılıyor).
8. *Katman 4 (crystallization)* sonraki tur. → C2'de uygulandı (§4c).

MMO test yatağı (gerçek dört shard aktörü): seam ötesi mob'a vuruş
sahibinde uygulanır, ikinci vuruş öldürür, kredi saldırana, mob iki
taraftan da kaybolur; aynı efektin iki teslimi bir kez uygulanır; sahte
hasar tavanlanır, bayat/çözülemez/menzil dışı reddedilir; iki farklı
shard'dan aynı tick'teki vuruşlarda düşük wire her zaman öldürür;
karşılıklı düelloda iki oyuncu aynı tick'te düşer ve iki oda aynı kaydı
üretir; park edilmiş karaktere seam ötesinden vurulunca kendi shard'ı onu
savaşta işaretler, çıkış vetosu bekletir. Yük üretecinin MMO botu
waystone çevresinde dolaştığından (seam'lerden 256 m) yükte seam ötesi
dövüş olmuyor; A/B yeni fazların boşta maliyetini ölçer (gürültü
içinde).

## 4c. C2 sonucu — crystallization, histeresizli (branch `xseam/c2-crystallize`)

§4'ün 4. katmanı uygulandı; seam ötesi etkileşim paketinin son parçası.
**Çekirdek değişmedi** (`gsb-core` diff'i boş). Kod: kit
`gsb-kit/src/sharded/crystal{.rs,/book.rs,/tick.rs}` (politika, dövüş
tablosu, tick geçişi), `sharded/room/shard.rs` (`collect_migrations`'ın
karar kuralı, varış/ayrılış), `sharded/mig.rs` (`ShardPin`),
`sharded/seam.rs` (`Seam::contact`), `space/partition{.rs,/grid.rs}`
(`Partition::holds`); MMO `world::CRYSTALLIZE` + `combat.rs`; sunucu
`[mmo] crystallize`; loadgen `--mmo-duel-frac`, `--mmo-crystallize`.

**1. Tespit nerede — kit.** Kit bir dövüşün iki yönünü de zaten görüyor:
giden etki `Seam::emit`'ten, gelen etki odanın `apply_remote_effect`'inden
geçer; partition (bölge, bant) ve göç kararı (`collect_migrations`) da
kit'te. Tek shard'ın kit'i yeter: yüksek wire'ın sahibi çiftin iki
yönünü de kendi kancalarında görür. Elenen: *çekirdek* — `RemoteEffect`'i
çekirdek de görür ama konumu, bölgeyi, partition'ı bilmez; karar
verseydi logic'e yeni bir "şunu şuraya taşı" kancası gerekirdi, ki bu
`collect_migrations`'ın zaten işidir; *oyun* — her oyun aynı tabloyu,
mover kuralını ve pin taşımayı yeniden yazardı, pin de kit'in göç
durumuna (`KitMig`) biner.

**2. Sinyal.** Kontak = kit'in gördüğü bir etkileşim: başarılı
`Seam::emit` (ya da `Local` reddi — emit-önce yazan oyun için), `Applied`
dönen uzak etki, ve oyunun `Seam::contact(source, target)` raporu (kit'in
göremediği YEREL darbe). Sırasız wire çifti başına
`Fight { since, last, up, down }`: ardışık iki kontak arası ≤ `window`
ise seri sürer, daha uzun sessizlik yeni seri açar. **Olgun:** serinin
kendisi K tick'e yayılmış (`last − since ≥ K`) VE iki yön de son
`window` içinde. Yerel-yerel çift tabloya girmez (taşınacak bir şey yok;
yalnız pin saatini tazeler). **Sınırlı durum:** tablo en çok
`FIGHT_CAP` = 1024 çift (tavanı aşan kontak `untracked` sayılır — o çift
kristalleşmez, uzak etkiyle doğru dövüşmeye devam eder); her tick
`window`'dan eski çift düşer, yani boyut = son `window` içinde temasta
olan çift sayısı, geçmiş değil. Pin entity başına biri; entity giderse,
ölürse düşer. Elenen: *tek yön* (sniper, DoT, karşılık vermeyen mob —
§2/§3'ün uzak-etki deseni zaten doğru cevap; taşımak fayda getirmez);
*yakınlık* ("K tick'tir sınırdan ayrılmıyor") — kit mesafe bilmez ve
yakınlık dövüş değildir; *serinin yaşı* (`since + K ≤ tick`) — K'dan
kısa bir alışveriş sessizlik penceresi içindeyse K'da olgunlaşırdı
(mutasyonla doğrulandı).

**3. Kim taşınır, nereye.** Çiftin **yüksek** wire'ı, alçak wire'ı ÖDÜNÇ
VEREN shard'a (`Lent::lender` — daima komşu, tek atlama). Wire kimliği
enkarnasyon boyunca değişmez ve iki shard ikisini de bilir: mesajsız,
kilitsiz anlaşma; yalnız yüksek wire'ın sahibi harekete geçer, karşı
taraf hiçbir şey yapmaz. Birden çok olgun partneri olan mover EN DÜŞÜK
wire'ı izler (olgun çiftler sıralı işlenir), tutulan (pinli) entity asla
mover olmaz. Elenen: *seam'e yakın olan taşınır* — iki shard birbirini
≤ 1 tick bayat ödünç kopyadan görür; eşitlikte ya da bayatlıkta ikisi
birden taşınıp yer değiştirir (swap → yine seam ötesi → ping-pong) ya da
hiçbiri; *yükü az olan shard'a* — komşunun yükü bilinmez, bilmek mesaj
ve uzlaşma ister; *rastgele* — deterministik değil.

**4. Sahiplik ↔ bölge (can alıcı karar).** Yalnız sahipliği değiştirmek
yetmez: mover konumunda duruyor, bir sonraki tick `region_of` onu eski
shard'ına geri verir — ping-pong'un ta kendisi (mutasyon: `anchor`'ı yok
say → dört test kırılır). **Karar: tutulan çiftin sahipliği
`region_of`'tan ayrıştırılır.** `collect_migrations` önce pin'in
`anchor`'ını sorar, yoksa `region_of`'u. Pin (`ShardPin { partner, last }`)
mover'la `KitMig.pin` içinde gider — park kaydı ve girdi oturumunun
bindiği aynı yol; yeni transfer mekanizması yok, çekirdeğin Migrate
protokolü (exactly-once, dolu kutuda geri alma) aynen. Alıcı shard hem
mover'ı hem partnerini kendine pinler (partner seam'i geçip karşıya
yürürse o da göç etmesin — çift tutulur). Partner varışta yerel değilse
(aynı tick'te başka yere gitmiş) pin kurulmaz, bölge sahibi olur — tek
geri dönüş, nadir yarış. Elenen: *mover'ı seam'in öte yanına itmek*
(oyunda görünür ışınlanma; oyuncu geri yürür, yine göç); *seam'i dinamik
kaydırmak* (bütün shard'ların partition üzerinde anlaşması = dağıtılmış
uzlaşma, aktör ilkesine aykırı); *ortak otorite* (§4 zaten eledi).
Görünürlük: tutulan entity yabancı bölgede durur; `GridPartition2`
bölgesi dışındaki her konumu `exports` eder, yani holding shard onu
şeride koyar ve bölge sahibinin istemcileri görmeye devam eder.

**5. Histerezis bandı.**

- *Zaman:* tutma, dövüş `release` tick sessiz kalana dek sürer; her
  kontak (uzak etki ya da `Seam::contact`'la yerel darbe) saati sıfırlar.
  Bırakılınca bölge sahipliği döner (gerekirse TEK göç).
- *Uzay:* `Partition::holds(idx, pos, margin)` — bölgenin içi ya da
  dışında `margin`'den az. Tutulan entity bandın dışına çıkarsa dövüş
  sürse de bırakılır (holding shard'ın çevresini ödünç almadığı yere
  gitti). **Giriş yarım bantla:** mover ancak hedef shard'ın
  `margin / 2` bandındaysa taşınır — bandın kenarından bırakılan entity
  dövüşmeye devam etse de `margin/2` ile `margin` arasında yeniden
  pinlenmez; kenarda salınım yok. `GridPartition2` margin'i border
  margin'ine kırpar (ötesini holding shard görmez); varsayılan `holds`
  her yerde evet der (geometrisi olmayan partition yalnız zamanla
  bırakır).
- *Partner:* holding shard'da partner kalmadıysa (öldü, çıktı, başka
  yere gitti) hemen bırakılır.
- *Salınımsızlık:* taşınma yalnız pinsiz yüksek wire'dan düşük wire'ın
  shard'ına; pinli entity taşınmaz; pin dövüş bitince, bant ya da partner
  yüzünden biter; yeniden kristalleşme yeni bir K-serisi ister, bant
  kenarından bırakılan `margin/2`'ye dönmeden pinlenmez. Durağan bir
  dövüş (kit testi: 300 tick, mover seam'in iki yanında gezerken)
  tam bir gidiş ve bir dönüş üretir.
- *Üç+ entity, köşe:* her mover olgun partnerlerinin en düşüğünü izler;
  en düşük wire o dövüş için hiç taşınmaz → dövüş onun shard'ında
  toplanır (köşe testi: C, A'nın shard'ına gider; orada tutulurken
  başka seam'den Z ile uzun dövüş onu taşımaz). Kayıp entity yok: her
  taşınma mevcut Migrate protokolüdür.

**6. Politika.** Opt-in oda builder'ı: `ShardedRoom::with_crystallize(
Crystallize)` (spatial composite'te de). `Crystallize { after, window,
release, margin }`, `Default` = 30 / 30 / 90 tick / ∞ (partition kırpar).
Açılmayan oda hiçbir durum tutmaz (`crystal: None`), yalnız bölgeyle
taşır, pin taşımaz (test). MMO: `world::CRYSTALLIZE` = K 30 (1 sn),
window 60, release 90, margin 64 m — bir AOI hücresi: tutulan karakterin
3×3 görünümü 128 m'lik ödünç şeridin içinde kalır; giriş bandı 32 m ≥
30 m menzil (seam'de melee düellosu hep içeride). Sunucu:
`[mmo] crystallize = true|false` (vars. true); `mmo_shard_with(i, realm,
None)` kapalı kurar. Elenen: *`ShardGame` kancası*
(`fn crystallize(&self) -> Option<Crystallize>`) — parametreleri oyun
bilir, ama aynı oyunu açık/kapalı koşturmak (A/B, sunucu config'i) oyun
tipine bir anahtar daha eklerdi; oda builder'ı mevcut politika deseni
(`with_disconnect_policy`), oyun kendi sabitini verir.

**7. C1 ile etkileşim, devir tick'i.** Kristalleşmeden sonra çiftin
etkileri YEREL: MMO yerel darbeyi dünya sorgusuyla indirir (`emit`
çağırmaz) ve `Seam::contact` ile bildirir. Devir: mover `h`'nin migrate
fazında gider. Partnerin `h`'de mover'a attığı darbe eski shard'a varır;
C1'in yönlendirme kaydı (`h`'de yazıldı, TTL 11) onu yeni sahibe iletir
ve `h + 2`'de BİR kez uygulanır — eski shard'da bir tick daha duran
ölümlü kopyaya asla (doğrulandı: MMO testi; yönlendirmeyi kapatan
mutasyon testi kırar). Mover'ın `h`'deki darbesi partnerin shard'ına
`h + 1`'de varır; mover da `h + 1`'in drain'inde oraya kurulur, yani
otorite kaynağı `local` olarak görür.

**§4'e göre sapmalar (gerekçeli):**

1. *"İki oyuncu K tick'tir sınırdan ayrılmıyor"* → konum değil ETKİ
   akışı: K tick'e yayılmış iki yönlü kontak serisi (madde 2).
2. *"Taraflardan biri proaktif olarak karşıya migrate edilir"* → mover
   KONUMUNU korur; değişen sahipliktir ve pin onu bölgeden ayrıştırır.
   "Karşıya" fiziksel değil.
3. *"Problem, zaten çözülmüş migrasyona dönüşür"* → doğru, ama göçün
   KARAR kuralı (`region_of`) pin'le genişledi; göç protokolü (çekirdek)
   değişmedi.
4. Tek yönlü sürekli etki kristalleşmez — §2/§3'ün uzak-etki deseni
   olarak kalır.
5. Sayaçlar metrik raporuna girmedi: kit olayları `gsb_kit::crystal`
   hedefinde debug satırı (`crystal_move`, `crystal_release
   why=Quiet|Band|Partner`); ölçüm bu satırları sayar (kit `tracing`
   bağımlılığı aldı).

**Testler.** Kit (fikstür oyun, `SeamStage` üzerinden, 8 + 1):
K'dan önce değil tam `1 + K`'da tespit, yalnız yüksek wire taşınır (alçak
wire'ın shard'ı hiç taşımaz); tek yön ve kısa alışveriş kristalleşmez;
açılmayan oda aynı davranır (durum yok, pin yok, bölge göçü aynı);
tablo tavanı ve süresi (3 × tavan kontak → tavan; pencereden sonra boş;
giden entity'nin pini düşer); 300 tick'lik tutulan dövüşte mover seam'in
iki yanında gezerken taşınma yok, dövüş bitince `release + 1` tick sonra
yalnız mover tek kez döner, sonra hiçbir şey; bant (yarım bant dışında
taşınmaz, margin'de bırakılır, bırakılan yeniden pinlenmez); partner
gidince bırakma; köşede üç dövüşçü + pinli entity'nin taşınmaması;
`GridPartition2::holds` (bölge, eksen, köşe, kırpma, varsayılan).
Önce-kırmızı: özellik kapalıyken (tick geçişi atlanınca) 8 kit
testinin 6'sı kırılır (kalan ikisi "hiçbir şey taşınmaz" iddiası). 21
kit mutasyonu + 4 `holds` mutasyonu yakalandı (anchor'ı yok say, seri
yayılımı yerine yaş, tek yön yeter, alçak wire taşınır, varış pinlemez,
sessizlik hiç, bant bırakması yok, tam bantla giriş, pinli mover olur,
tavan yok, süre dolması yok, gelen etki sayılmaz, giden sayılmaz, en
yüksek partner, `contact` boş, partner bırakması yok, seri sıfırlanmaz,
ayrılış pini silmez, pin taşınmaz, partner pinlenmez; kırpma yok,
`<=`, bölge kontrolü yok, varsayılan hayır). MMO (gerçek dört shard
aktörü, `cross_seam_crystal.rs`): düello x = 0'da tam `h = s + 1 + K`'da
Q'yu P'nin shard'ına taşır (üyeler `[1,1] → [1,0] → [2,0]`), her tick
iki oyuncu birbirini görür, her darbe bir kez uygulanır (P'nin `h`
darbesi yönlendirilip `h + 2`'de; Q'nun tümü P'nin shard'ında), kalan
hp 25/25; son darbeden `release + 1` tick sonra Q bölgesine tek kez
döner ve 300 tick salınım yok; tek darbe ve kısa alışveriş kimseyi
taşımaz. Önce-kırmızı: MMO politikası kapalıyken düello testi kırılır;
MMO'nun yerel `contact` raporu kaldırılınca (bırakma erken gelir) ve
çekirdeğin yönlendirmesi kapatılınca (darbe ölümlü kopyaya) kırılır.
Sunucu: `[mmo] crystallize` okunur/reddedilir. Loadgen: düellocu
seçimi/konumu, girdi karışımı, düellocu olmayan botun girdisi bayrakla
bayt-bayt aynı; bayrakların oyun kısıtı; override `[mmo]` tablosuna
yazılır.

**Yük ölçümü.** Loadgen'in MMO botu seam'den uzakta dolaştığından
`--mmo-duel-frac F` eklendi (bkz. `loadgen/bot/mmo/duel.rs`): botların
F kesri (id çiftleri) x = 0 seam'inin iki yanında 8 m'de, 32 m arayla
16 noktada karşılıklı saldırır; yenilen waystone'dan geri yürür. F = 0
(varsayılan) botu değiştirmez (test). Yürüyüş ~35 sn sürdüğü için
koşular 90 sn; sayılar koşunun tamamı üzerinden (ilk ~35 sn düellocular
yürür).

Komut: `RUST_LOG=warn,gsb_core::shard::actor::tick::effects=info,gsb_kit::crystal=debug
gsb-loadgen N --game mmo --duration 90 --mmo-duel-frac 0.2 --mmo-crystallize on|off
--write-stall-secs 0` (release, süreç içi, 32 çekirdek, iki tur, on/off dönüşümlü;
koşu öncesi 1 dk yük ortalaması 3,7–6,2). Uzak etki = dört shard'ın son
`remote_effect_summary` toplamı; crystal göçü = `crystal_move` satırı.
Her koşuda `joined = left = N`, `errors=0`, `server_closes=0`.

| N | crystallize | `server_hz` | step p50/p90 fine (µs) | `out_bps_per_conn` | uygulanan uzak etki (/sn) | crystal göçü (/sn) | bırakma quiet/band/partner | wire başına en çok göç |
|---|---|---|---|---|---|---|---|---|
| 200 | on | 30,00 · 30,00 | 152/208 · 144/192 | 16 159 · 16 198 | 165 · 179 (1,8–2,0) | 27 · 24 (0,27–0,30) | 32/10/11 · 24/11/11 | 2 · 2 |
| 200 | off | 30,00 · 30,00 | 152/216 · 152/200 | 16 101 · 16 164 | 225 · 227 (2,5) | 0 · 0 | — | — |
| 500 | on | 30,00 · 29,99 | 264/352 · 280/376 | 41 081 · 41 019 | 534 · 529 (5,9) | 36 · 25 (0,28–0,40) | 27/21/22 · 16/16/18 | 2 · 2 |
| 500 | off | 30,00 · 30,00 | 264/376 · 280/384 | 41 205 · 41 056 | 597 · 576 (6,4–6,6) | 0 · 0 | — | — |

- **Maliyet gürültü içinde:** `server_hz` 30, adım p50/p90 ve
  `out_bps_per_conn` on/off aynı aralıkta (±%0,3). Dövüş tablosu ve pin
  geçişi ölçülebilir iş eklemiyor.
- **Uzak etki azalıyor ama az:** 200'de ~%25, 500'de ~%9. Bu yükün
  dövüşleri kısa (dört darbe yenilgi, ~1 darbe/sn): kristalleşme K = 1 sn
  ve iki yön canlı olduktan sonra geliyor, dövüşün başı her durumda
  uzaktır; 500'de noktalar 3'e 3 kalabalık — mover EN DÜŞÜK partnerine
  gider, aynı noktadaki başka çiftler seam ötesinde kalır (tasarım
  gereği: bir pinli entity başka dövüşle taşınmaz).
- **Salınım yok:** 90 sn'de hiçbir wire kristalleşmeyle ikiden fazla
  taşınmadı (bir dövüş, bırakma, yeni dövüşte yeni seri).
  `band` + `partner` bırakmaları çoğunlukla yenilgidir: yenilen waystone'a
  ışınlanır (bant dışı), partneri de partnersiz kalır.
- **Göç/sn:** çekirdek göç sayısı raporlamıyor; crystal göçleri
  0,27–0,40/sn, bunun yanında varsayılan botların `Travel`'ı
  (~20 sn'de bir) 200'de ~8/sn, 500'de ~20/sn göç üretir.
- **Yan bulgu (crystallization'dan bağımsız, on/off eşit):** 500'de
  `snap_overflows` ~32 k — düellocular 16 noktada kümelenince hücre
  full'ları 1400 B tavanını aşıyor (G3'ün MMO 500 tabanında 4229).
  Kümelenmiş MMO yükünde snapshot bölme ihtiyacı; bu turun kapsamı dışı.

**Varsayılan yükte A/B (base `276041e` ↔ HEAD, dönüşümlü üç çift,
200 istemci, `--duration 10 --write-stall-secs 0`, yük 4,4–7,4):** üç
senaryoda da gürültü içinde; her koşuda `joined = left = 200`,
`errors=0`, `server_closes=0`, `server_hz` 29,99–30,00.

| Senaryo | step p50/p90 fine (µs) base | HEAD | `out_bps_per_conn` base | HEAD |
|---|---|---|---|---|
| demo sharded × spatial (`--topology sharded --visibility spatial --shard-count 4`) | 176/232 · 160/208 · 136/176 | 144/184 · 152/192 · 144/192 | 11 989 · 11 646 · 11 591 | 11 787 · 11 898 · 11 827 |
| demo sharded (`--visibility sharded`) | 96/128 · 112/168 · 128/176 | 96/128 · 128/160 · 96/128 | 28 509 · 28 202 · 28 353 | 28 220 · 28 356 · 28 596 |
| MMO varsayılan bot (`--game mmo`) | 128/168 · 128/160 · 136/184 | 128/168 · 120/160 · 128/168 | 20 732 · 20 649 · 20 732 | 20 515 · 20 554 · 20 456 |

MMO varsayılan botunda crystallization AÇIK (MMO'nun varsayılanı) ama
seam ötesi dövüş yok: ölçülen, boştaki maliyettir.

## 5. Ortak fizik (tutma/itme) — tasarım uyarısı

Tek sonucu iki otoritenin belirlediği mekanikler (grab, ortak push)
bu dokümanın çözmediği sınıftadır. Seçenekler: otorite-devri
(kim tutarsa fizik onun), tasarimsal kaçınma (böyle mekaniği sınıra
koymama), ya da ayrı bir uzlaşma protokolü. v1 kapsamı dışı.

## 6. Border paylaşımı: full'dan delta'ya önerisi

### 6.1 Mevcut durum

Her tick, faz 5: sınır şeridi TAM DURUM olarak tüm komşulara
(`BorderExchange`); alıcı en son exchange'i bütünüyle silip kurar
(`border: HashMap<komşu, Vec<BorrowedRecord>>`). Tam-durum olması
kendini-onarıcıdır: düşen/geciken exchange bir tick içinde telafi olur.

### 6.2 Dış öneri (kabul: değerlendiriliyor)

Full yerine **delta** + ara ara full düzeltme; her shard komşusunun
border snapshot'ını tutup gelen delta ile günceller.

**Gerekçe zinciri (dış danışmadan, kayda değer):**

- Sınır dibinde duran oyuncu karşı tarafa **max görüş mesafesi**
  kadar bakabilmeli → border genişliği küçük TUTULAMAZ; alt sınırı
  oyunun görüş menzilidir.
- Devasa haritalar: ör. 10×10 = 100 shard. Çok shard × geniş şerit ×
  yoğun bölgeler (oyuncular POI'larda, POI'lar sıkça sınıra yakın
  dizayn edilir) → full-exchange O(yoğun şerit × komşu sayısı)'na
  ölçeklenir; delta O(değişen kayıt)'a iner.
- Süreçler arası dağıtımda (ufuktaki katman) şerit gerçek ağ üzerinden
  akar: gecikme/kayıp/dar bant delta'yı "güzel olur"dan "gerekli"ye
  taşır.

### 6.3 Düzeltilmiş varsayım: "in-process kayıpsız" DEĞİL

Kanallar bounded `try_send`: komşu geride kalıp kuyruğu dolduğunda
delta sessizce DÜŞER. Full-exchange bunu umursamaz (sonraki full bayatı
silip yeniden kurar); delta ise kalıcı sapma üretir. Dolayısıyla
delta tasarımı kayıp-kurtarma olmadan tamamlanmış sayılmaz.

### 6.4 Sağlam delta tasarımı (dört pin)

1. **Seq-damgalı delta:** her exchange monoton seq taşır; alıcı beklenen
   seq'i tutar, gap görürse resync tetikler.
2. **Çıkış kayıtları:** delta "girdi/değişti"nin yanında "çıktı"yı da
   taşır (AOI CellExit deseni) — yoksa hayalet kalıcılaşır.
3. **Üç resync tetikleyicisi:** (a) seq gap; (b) komşu rebuild bildirimi
   (supervision RoomDied akışı zaten var — yeni enkarnasyon ilk tick'te
   full gönderir); (c) sigorta olarak düşük-frekans periyodik full
   (istemci tarafındaki keepalive-full karşılığı; örn. her 256 tick).
4. **Migrasyon etkileşimi:** varlık sınırı geçtiğinde eski tarafın
   deltasında çıkış, yenilerinde giriş doğal görünür; own-wins filtresi
   geçiş tick'indeki çift-görünümü yutar.

### 6.5 Tam-shard border (her şeyi paylaşmak) — ELENDİ ❌

Border'ı tüm shard'ı kapsatacak kadar genişletmek fonksiyonel olarak
çalışır ama amaçları yok eder: her shard her tick dünyanın bütününü
decode/encode/re-kodlar → iş yükü O(tüm dünya)'ya çıkar, paralellik
kazancı kanal trafiğinin arkasından geri alınır; "no change" defteri
sürekli kirli kalacağından sessiz-tick tasarrufu da gider. Sonuç:
tek odadan pahalı bir sharding. Border genişliği oyun-parametresidir
(görüş menzili alt sınırı); dünya-genişliği paylaşıma dönüşmez.

## 7. Ölçüm planı ve sonuçları (Faz 0 + C1 + C2 TAMAMLANDI)

Enstrümantasyon: `ShardActor` üzerinde `BorderStats` (faz-5 duvar süresi,
dışa aktarılan kayıt/byte, try_send drop; alıcı tarafında apply süresi) —
~1 sn'de bir `border_exchange_summary` info! satırı. Ham loglar
`target/border-baseline/` altında (SUMMARY.md, SUMMARY-C1.md,
SUMMARY-C2.md, c1/c2.out/.err).

### Ölçümler (release, orchestrator --procs 4 --pin, spread, 30 sn)

| Koşu | Grid | İstemci | Hücre | Derinlik | rec/tick/shard | KB/tick/shard | faz-5 µs/tick |
|---|---|---|---|---|---|---|---|
| B1 | 2×2 | 2000 (500/sh) | 1000² | 250 | 568–680 | 9–11 | 1.0–1.2 |
| B2alt | 2×2 | 4000 (1000/sh) | 1000² | 250 | 1212–1399 | 19–22 | 2.2–2.5 |
| B2 | 2×2 | 8000 | 1000² | 250 | 2569–2790 | 41–45 | 5.5–5.8 |
| C1 | 5×5 | 5000 (200/sh, sabit harita!) | 400² | 100 | ~495 (mean) | 7.9 | 3.0 |
| C2 | 5×5 | 12500 (500/sh, **büyüyen dünya**) | 1000² | 250 | ~1193 (mean) | 19.1 | 5.1 (mean; iç shard 7.0) |

### Bulgular

1. **C1 (sabit harita, hücreler küçülür):** demo derinliği hücreyle
   orantılı olduğundan (`min(cell)/4`) per-shard maliyet DÜŞER — model
   `rec ≈ 0.75·(M/N)·komşu` iki baseline'ı gürültü payında tahmin eder.
   Demo parametizasyonunda büyüme baskısı YOK.
2. **C2 (dünya büyür, hücre sabit):** dış danışmanın öngörüleri
   DOĞRULANDI — (A) exchange başına set boyu flat (~300–411 vs B1'in
   ~322); (B) toplamlar komşu sayısıyla ölçeklenir (köşe/kenar/iç =
   9.5/16.9/26.4 KB, yani ~2:3:4). Grid ortalaması 19.1 KB/tick/shard ≈
   B1'in 1.8×'i (ort. komşu 3.2 vs 2).
3. **Sistem tavanı:** 25 shard + 12.5k bağlantı 30 Hz'de stabil
   (ticker fan-out 25 abone, lag yok; registry temiz). 25k istemci
   loadgen'in client-decode duvarına çarptı (harness sınırı).
4. **Gerçek-oyun ekstrapolasyonu:** sabit dünya-birimli görüş menzili
   olan gerçek oyunlarda hücre küçüldükçe şerit hücrenin büyüyen
   kesimini kapsar → C2'nin gördüğü komşu-sayısı büyümesine EK olarak
   şerit-yoğunluğu büyümesi gelir; süreçler arası dağıtımda bu byte'lar
   gerçek ağa çıkar. Delta'nın güçlü gerekçesi bu iki senaryodur;
   in-process demo ölçeklerinde full-exchange kabul edilebilir kalır.

### Karar (A/B ölçümü sonrası güncellendi): delta KABUL EDİLDİ ✅

> **Güncel durum — Faz C bu kararı süreç-içi için geçersiz kıldı.**
> Aşağıdaki karar merge anındaki (`ddd38a6`, 2026-08-25) kayıttır ve
> olduğu gibi korunuyor. Aynı gün gelen Faz C (`f431296`) paketlemeyi
> link sınıfına bağladı: bugün var olan TEK link türü `InProcLink`,
> sabit olarak `ExchangeMode::AlwaysFull` ilan eder
> (`crates/gsb-core/src/shard/link.rs`). Gerekçe bu bölümün kendi
> ölçümü: süreç içinde byte bir mpsc taşımasıdır, bedeli yoktur; delta
> diff'i ise faz-5'in CPU'sunu 40–55× artırır (tablodaki µs sütunu).
> Delta, henüz var olmayan `Ipc`/`Net` link'lerine ayrılmıştır.
>
> Sonuç: delta border kodu main'de ama **hiçbir çalışan konfigürasyon
> onu kullanmıyor** — yalnız testler, crate-içi `force_exchange_modes`
> kolu ile Delta-modlu rig kurarak çalıştırıyor
> (`shard/tests/border.rs`, `strip/modes.rs`, `rigs.rs`). Kod uykudadır;
> ilk gerçek tüketicisi DISTRIBUTED'daki süreçler-arası link olacaktır.
> Aşağıdaki "always-full … elenen alternatif" cümlesi de bu yüzden
> geçersizdir: always-full elenmedi, süreç-içinin tek modu oldu.

`border-delta` branch'inde dört-pinli tasarım implement edildi ve aynı
enstrümantasyonla A/B ölçüldü (B1'/B2alt'/C2' — baseline B1/B2alt/C2 ile
birebir senaryolar):

| Koşu | delta byte/tick/shard | eşdeğer full | oran | faz-5 µs (base→delta) |
|---|---|---|---|---|
| B1' | 4.0 KB | 10.1 KB | 0.40× | 1.1 → 48 |
| B2alt' | 8.4 KB | 21.3 KB | 0.39× | 2.4 → 138 |
| C2' | 7.5 KB | 19.1 KB | 0.39× | 5.1 → 227 |

- Byte kazancı üç şekilde de tutarlı: **~%60 azalma** — dağıtım
  senaryosunun para birimi.
- CPU bedeli gerçek ama bütçenin <%1'i (en kötü şekil +417 µs step mean);
  diff+map-apply, byte tasarrufuyla takas edilmiş.
- Sıfır steady-state drop/resync; düşen deltalar gönderici-taraflı
  forced-Full ile bir tick içinde iyileşti (resync_requests_sent=0).
- Doğruluk kilidi: 6 yeni test (upsert/exit-hayalet yokluğu,
  seq-gap→resync, rebuild-leads-with-Full, kadans, send-failure flag,
  own-wins). Test 231 → 237.

**Pluggable sorusu (dış danışma) — değerlendirildi, şimdilik hayır:**
Mesaj tipi zaten `Full | Delta` birleşimi olduğundan alıcı tarafı çok
biçimli; mod seçimi gönderici-yerel olabilir. Ancak A/B, delta'nın
her ölçekte makul çalıştığını gösterdiği için ikinci modun yaşatılmasına
gerek kalmadı — "always-full" kaçış kapısı gerekirse sonradan config
anahtarı olarak eklenebilir (yarım gün), şimdilik elenen alternatif.

**Dağıtım notu:** UDS/kablo ortamlarında güvenilir-sıralı link
(`ShardLink`, DISTRIBUTED.md adayı) delta'nın resync yollarını
fiilen devre dışı bırakır — bu İYİDİR: karmaşıklık ağın olduğu yerde
yaşar, loopback'te yaşmaz.

1. **Faz 0 (main):** mevcut BORDER fazını enstrümante et — byte/tick,
   kayıt/tick, encode µs, send-drop sayacı; sharded senaryolarda ölç
   (4 shard orta yoğunluk + orchestrator yüksek yoğunluk). Ham sayılar
   ROADMAP'e işlenir.
2. **Faz 1 (branch `border-delta`):** §6.4 tasarımının implementasyonu,
   aynı enstrümantasyonla aynı senaryolar.
3. **Karar kriteri:** kazanç ölçülebilir VE karmaşıklık bedeline değiyorsa
   branch main'e alınır; değilse ölçüm kaydıyla branch arşivlenir.
   (Proje ilkesi: önce veri.)

## 8. team × sharded kompoziti: registry-hub takım-export (tasarım hazır)

Problem: takım üyeleri shard'lar arasına SAÇILMIŞ durumda — border
ödünç vermesi yalnız sınır şeridini kapsar; sağ shard'daki takım
arkadaşını sol shard'daki oyuncu göremez. Lokalite-karşıtı ilgi:
cross-shard abonelik/yayın katmanı ister.

### 8.1 Karar: byte-encoded registry-hub anti-entropy

Shard'lar takım üyesi kayıtlarını REGISTRY hub'ına export eder; registry
o takıma üye barındıran tüm shard'lara AGREGATE diğer-shard kayıtlarını
fan-out eder.

**Kritik tasarım kararı — kayıtlar registry'ye VERİLMEDEN ÖNCE encode
edilir:** `TeamExport { room, from_shard, records: Vec<(u64 takım, u64
wire, Bytes)> }` — RegistryMsg monomorfiktir ve generic'e çevirmek 50+
kullanım noktasında dalgalanma demektir (ELENDİ). Byte-encoded olması,
DISTRIBUTED §4b ilkesinin ("tipi kim tanımlıyorsa codec'i de o tanımlar")
buradaki uygulamasıdır: alıcı logic kendi codec'iyle çözer, registry
içeriği hiç bilmez.

### 8.2 Hub tabloları ve kurallar

- Tablo: `HashMap<(RoomId, takım), HashMap<from_shard, (kayıtlar,
  son_refresh_tick)>>` — export'ta wholesale-replace (en-son kazanır).
- **Expiry sweep:** refresh olmayan kayıt `TEAM_EXPORT_TTL_TICKS`
  (örn. 256, tombstone deseni) aşılınca amortize retain ile düşer —
  üye ayrıldığında hayalet kalıcılaşmaz; drop sonrası da sonraki
  export kendini onarır.
- **Fan-out:** takım T için export gönderen shard KÜMESİNİN her üyesine,
  DIĞER shard'lardan gelen agregate kayıtlar `try_send` ile iletilir
  (best-effort; drop = sonraki tick'in export'u onarır).
- **İzolasyon:** takım A'nın kayıtları yalnız A'yı görüntüleyenlere
  gider; B görüntüleyicisine sızmaz (test kilidi zorunlu).

### 8.3 Alıcı taraf

Gelen agregate kayıtlar shard'da takım-başına saklanır ve snapshot
seam'ine ek girdi olarak akar (borrowed şeritle paralel imported-set;
GameLogic imzasına minimal ek — ikinci slice parametresi). Görüntüleyici
takımı eşleşmeyen kayıtlar logic tarafından filtrelenir.

### 8.4 Bedel ve sınırlar

- Tablo büyüklüğü: takım-sayısı × takım-büyüklüğü × shard-sayısı ile
  sınırlıdır (MOBA takımları ~5; guild ölçeğinde config'li üst sınır).
- Export maliyeti: O(kendi takım üyeleri) — dünya-geneli değil.
- v1 dışı: cross-seam ETKİLEŞİM (sadece görünürlük); otomatik balancer;
  kalıcılık entegrasyonu.

## 9. Uygulama durumları

| Kalem | Durum |
|---|---|
| Delta border exchange (§6.4) | 💤 main'de ama **uykuda** — Faz C'den beri süreç-içi link'ler `AlwaysFull`; delta yalnız testlerde (`force_exchange_modes`) koşar, ilk tüketici `Ipc`/`Net` link'i (§7 "Güncel durum") |
| sharded × spatial kompoziti | ✅ Faz B — main'de |
| Ortak delta motoru çıkarımı | ◐ CellBook/CellPieces common.rs'te; strateji adoptasyonu tetikleyicili |
| team × sharded (bu bölüm) | 🔜 Tasarım hazır — taze oturumda uygulanır |
| Çoklu-listener (karışık transport istemci) | ✅ ROADMAP — uygulandı |
| Seam ötesi okuma + `RemoteEffect` (§2, §4 katman 1–3) | ✅ C1 — §4b |
| Crystallization (§4 katman 4) | ✅ C2 — §4c (opt-in; MMO açık) |

## 8. NOT-DONE

- ~~Cross-seam combat/interaction mesaj tiplerinin implementasyonu~~ —
  C1'de uygulandı (§4b); crystallization C2'de (§4c); kalan:
  saldırı-anında güncel borrow (katman 1'in seçeneği)
- Ortak fizik uzlaşması (§5)
- Adaptif border genişliği (ölçüm öncesi optimizasyon)
- Dağıtımlı (multi-process) shard topolojisi — ufuk katmanı
