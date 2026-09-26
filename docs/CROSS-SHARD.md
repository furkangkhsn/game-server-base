# gsb: Cross-Shard Etkileşim ve Border Paylaşım Tasarımı

> Durum: TASARIM NOTU (dış danışma diyaloğundan derlendi); §2–§4
> UYGULANDI — uzak etki §4b (C1), crystallization §4c (C2), göç tick'i
> düzeltmesi §4d (D). §2–§5 etkileşim
> desenleridir; ~~§6–§8~~ §6–§7 border paylaşımının delta'ya evrimi ve ölçüm planıdır
> ~~(ölçüm turu yürütülüyor)~~ *(ölçüm tamamlandı — §7; delta border kodu
> main'de ama uykuda, §9)*; §8–§8b team × sharded kompozitidir (W1/W2
> uygulandı — §8b). Uygulama turları bu dokümanı sözleşme alır.

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
(crystallization) sonraki turdur *(C2'de uygulandı — §4c)*. Kod: `gsb-core/src/shard/{effect,seam}.rs`,
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
  darbe kaybı); broadcast + epoch guard (aşağıda sapma 3). (Bu UZAK
  etkiyi korur; ölümlü kopyaya o tick'te eski shard'ın kendi YEREL
  darbesi D'ye kadar kayboluyordu — §4d.)
- **Atıf:** `source` zarfın içinde; otorite (MMO) kill kredisini ona
  yazar.
- **Anti-cheat yerelliği:** menzil saldıranın shard'ında ödünç kayda
  karşı; MMO otoritesi politika olarak yeniden denetler (bayatlık 3
  tick, saldıranı gördüğü yerden menzil + 2 m pay, hasar tavanı).
- **Sayaçlar:** shard başına kümülatif `EffectStats`, ~1 sn'de bir
  değiştiyse `remote_effect_summary` info satırı. *Küçük paketten beri
  metrik raporunda da:* `RoomSample`/`RoomReport`'ta operatöre dönük beş
  sayaç — `effects_applied`, `effects_forwarded`, `effects_orphaned`,
  `effects_dropped` (yolda kayıp: dolu yeniden deneme tamponu + kapalı
  link + hop sınırı + yaş sınırı), `effects_refused` (kaynakta `emit`
  reddi: bütçe ya da ödünç verilmeyen hedef); gsb-metric satırı,
  Prometheus `gsb_room_effects_*_total` (OPS §3), loadgen fold'unda SUM.
  İnce döküm (`sent`, `retried`, `rejected`, `duplicates`, `foreign`…)
  log satırında kaldı: operatör sorusu "etkiler uygulanıyor mu, kayıp
  var mı"dır; on beş sayaç raporu şişirirdi.

**§2–§4'e göre sapmalar (gerekçeli):**

1. *`target_epoch`* ayrı bir hedef-epoch'u değil, efekt kimliğindeki
   ODA ENKARNASYONU epoch'udur (registry'nin kurulum nesli). Wire
   kimlikleri bir enkarnasyon boyunca asla yeniden kullanılmaz (shard'ın
   sayacı geri gitmez, shard'lar ayrık sınıflar basar — A30'dan beri iç
   içe basım, önceden aralık bölümleme — ve göçte kimlik korunur), yani
   hedef kimliği tam olarak `(wire, epoch)`; hedef-başına bir epoch hiç
   bilgi taşımazdı.
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
ve uzlaşma ister; *rastgele* — deterministik değil. (A30'dan beri wire
id'leri iç içe basılıyor: "yüksek wire" artık "yüksek indeksli shard'ın
bastığı" değil, kabaca "aynı sayıda çekimde daha geç çekilen" demek.
Kural, deterministikliği ve iki shard'ın mesajsız anlaşması aynı;
yalnız bir çiftte hangi tarafın taşındığı değişebilir.)

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
otorite kaynağı `local` olarak görür. **Düzeltme (D, §4d):** yukarıdaki
yalnız UZAK darbe için doğruydu. Devrin YEREL yarısı — ölümlü kopyaya
eski shard'ın `h + 1`'deki yerel darbesi (kristal bırakma / bant / partner
göçünde tutulan partnerin darbesi; herhangi bir göçte üçüncü bir yerel
saldıran) — kopyaya iniyor ve kayboluyordu; D'den beri yeni sahibe
yönlenir ve `h + 2`'de bir kez uygulanır (MMO testi `migration_tick.rs`).

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
   bağımlılığı aldı). *Küçük pakette yeniden değerlendirildi, log
   satırı olarak KALDI:* çekirdeğin `RoomSample`'ı sabit biçimli bir
   `Copy` yapıdır ve her alanı adıyla fold kuralı, render anahtarı,
   Prometheus ailesi ve loadgen codec'i taşır; kit'e özgü bir olay için
   ya çekirdeğe kit kavramı sızar (`crystal_moves` alanı — çekirdek
   crystallization'ı bilmez), ya da genel bir "oyun/kit sayacı" seam'i
   gerekir (`GameLogic`'ten adlandırılmış sabit boy bir dizi: her
   örnekte her odaya N×8 bayt, dinamik Prometheus adları, alan başına
   bilinmeyen fold kuralı). Crystallization opt-in ve ölçüm aracı;
   tek tüketicisi ölçüm koşuları, onlar da debug satırını sayıyor.
   *Tetikleyici:* canlı bir sunucuda operatörün crystal sayısına
   ihtiyacı olması ya da ikinci bir kit-tarafı olay ailesi — o gün genel
   seam kendini öder. Göç SAYISI ise çekirdeğin olayıdır ve rapora girdi
   (`migrations_out/in/failed`, aşağıda §4d).
   *F9'da kapandı (2026-09-26):* genel seam yapıldı —
   `GameLogic::logic_counters` + `LogicCounters` (çekirdek kit
   kavramı öğrenmedi; sabit boy dizi 16 yuva, adlar statik `const`,
   Prometheus'ta ad başına aile, fold kuralı sayaçla birlikte:
   DESIGN §12, OPS §3). Crystallization'ı açan sharded oda altı sayaç
   koyar: `crystal_moves`, `crystal_release_quiet`,
   `crystal_release_band`, `crystal_release_partner`,
   `crystal_untracked` (tavanın reddettiği kontak) ve
   `crystal_fights_peak` (tablonun tepe boyu, MAX); açmayan oda hiçbirini
   koymaz. Debug satırları wire başına ayrıntı için kaldı. MMO testi
   (`cross_seam_crystal.rs`) onları gerçek shard aktörlerinin
   örneklerinden okur; 200 botluk 60 sn `--mmo-duel-frac 0.2` koşusunda
   RESULT: `logic_crystal_moves=20 logic_crystal_release_quiet=19
   logic_crystal_release_band=10 logic_crystal_release_partner=11
   logic_crystal_untracked=0 logic_crystal_fights_peak=9`.

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
  → Taşımada çözüldü (rUDP parçalama turu, DESIGN §6 "MTU"): aşımların
  ~%96'sı delta, gerisi keep-alive full (tepe 2566 B); rUDP'de aynı
  koşu (`--transport udp --stagger-ms 5`) artık ~700 k mesajı 2'şer
  parçayla, `frag_dropped=0`, `joined = left = 500`, 0 kapanışla
  taşıyor (önce: ~10,4 k kare atılıyor, düellocuların çoğu kendini hiç
  görmüyordu, 206 oturum idle-sweep ile kapanıyordu).

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

## 4d. D sonucu — göç tick'i (branch `fix/d-migration-tick`)

C2 turunda bulunan, her göçte var olan (yeni olmayan) açık kapatıldı:
göç eden entity'nin eski shard'da bir tick daha duran **ölümlü
kopyası**na o tick'te inen YEREL darbe kayboluyordu. Uzak etkiler
C1'in yönlendirmesiyle zaten güvendeydi (§4b); yerel yol değildi.

**Pencere (faz sırası, eski shard A → yeni sahip B, entity e).**

| Tick | A | B |
|---|---|---|
| `h` | 0 drain · 0d etki · 1 okuma · 2b ingest · 2c istek · 3 sistemler · 3b etki çıkışı · 4a (önceki göçlerin despawn'ı) · **4b `collect_migrations` → `capture`** (sistemlerden SONRA: `h`'nin bütün yerel darbeleri durumun içinde) → `Migrate { at_tick: h }` gönderilir; başarılıysa `pending_out (e, h + 1)` ve yönlendirme `e → (B, h + 11)` · 5 border (e dahil) · 6 yayın (e A'nın kaydı) | — |
| `h + 1` | 0 drain · 0d: e'ye gelen uzak etki yönlendirilir (C1) · **2b ingest · 2c istek · 3 sistemler: kopya DÜNYADA** — dünya sorgusu onu bulur, `Seam::local` onu döndürür · 3b · **4a `on_migrate_out` → despawn** | 0 drain: kurulum (`at_tick h < h + 1` kapısı) — e B'nin |

Pencere = **A'nın `h + 1` tick'inin gövdesi, faz 0'dan faz 4a'ya
kadar**. İçinde kopyaya yapılan her yazım kaybolur (durumu `h`'de
yakalandı ve gitti), kopyanın kendi sistemleri ve — park edilmiş
oyuncuda — botu `h + 1`'i B'deki gerçek entity'nin YANINDA ikinci kez
oynatır. Oyuncu girdisi güvende: girdi kanalı `h`'de Migrate ile taşındı,
A `h + 1`'de kopya için girdi çekmez. Yeniden üretim (düzeltmeden önce,
gerçek dört shard): MMO düellosunda P'nin `h + 1` darbesi
`(shard 0, tick h + 1, hp 0, killed)` olarak kopyaya indi, Q shard 1'de
25 hp ile yaşadı; kristal bırakma devrinde (Q `out`'ta bölgesine döner,
P `out + 1`'de vurur) birebir aynısı.

**Karar: göç tick'inde kopya oyunun kancalarından çıkar, yeni sahibinin
ödünç kaydı olur.** A'nın `h + 1`'deki ilk seam kancasından (0d
`apply_remote_effect`, 2b `ingest_seam` ya da 3 `step`) oyunun sistemleri
bitene dek:

- kopya **`Disabled`** (Bevy'nin varsayılan sorgu filtresi): oyunun hiçbir
  sorgusu onu bulmaz — yerel darbe onu ıskalar, kendi sistemleri
  (hareket, AI, otomatik saldırı) onu koşmaz; 2c'nin istek kancası da
  içeride;
- `Seam` onu **B'den ödünç** gösterir, `h`'de yakalanan kayıtla:
  `local(e)` = `None`, `lent(e)` = `Lent { lender: B, state: yakalanan }`,
  `lent_iter` onu bir kez verir (B'nin ilk export'u gelmiş olsa da
  olmasa da AYNI cevap — zamanlama şansından bağımsız), `emit(e)` B'ye
  gider. "Yerel, değilse ödünç" çözen bir darbe (MMO'nun melee'si: dünya
  sorgusu → `lent` → `emit`) böylece A'da `at_tick = h + 1` damgalı
  sıradan bir etki olur ve B'de `h + 2`'de, C1'in kimliği, pencereli
  dedup'ı ve `(source, origin, seq)` sırasıyla **bir kez** uygulanır;
  kopyaya hiçbir şey yazılmaz;
- bot onu sürmez (B'nin botu sürer); ona `emit` seam ötesi kontak
  sayılır (crystal tablosu çifti görür, yerel-yerel sanmaz);
- sistemlerden sonra kopya geri gösterilir: kit'in kendi geçişleri
  (orphan damgalama, crystal değerlendirmesi, border export'u) tick'i
  eskisi gibi görür, çekirdek 4a'da eskisi gibi despawn eder.

Kod: kit `sharded/departing.rs` (`Departures`: yakalanan kayıtlar +
gizlenen kopyalar), `sharded/seam.rs`, `sharded/room{.rs,/shard.rs}`
(capture'da kayıt, kancalarda gizle/göster, migrate-out'ta unut),
`common/hooks.rs` (bot beslemesi), `sharded/crystal.rs` (kontak
sahiplik yüklemi). **Oyun API'si değişmedi; MMO'nun kodu değişmedi.**

**Çekirdek: tek küçük, kaçınılmaz ekleme.** Gönderimin commit'i yalnız
çekirdekte bilinir (link `try_send`'i; reddedilen gönderim entity'yi A'da
bırakır, satırı geri alınır, girdisi A'da çekilmeye devam eder) ve kit
bunu 4a'dan önce öğrenemez. `CrossSeam` A'nın zaten tuttuğu yönlendirme
tablosunu okur: `departed(wire) -> Option<usize>` (commit edilmiş göç,
TTL içinde, geri gelmemiş) ve `emit` kimsenin ödünç vermediği hedefi
oraya yollar (önce ödünç veren, eskisi gibi; `NotLent` yalnız ikisi de
bilmiyorsa). Test aracı: `SeamStage::depart(wire, to)`. Mesaj, faz, göç
protokolü değişmedi; tick gövdesi aynı boyda (`EffectBook::seam`).

**Simetrik durum ve crystallization devirleri.**

- *Mover'ın kendi eylemleri:* `h`'deki eylemi A'da yereldir ve yakalanan
  duruma girer (bir kez); `h + 1`'de girdisi B'de, botu B'de, sorgu-güdümlü
  sistemleri A'da kopyada koşmaz → ikinci uygulama yok (kit testi:
  otomatik saldırı yalnız B'den; MMO: Q'nun `h` darbesi A'da `h`'de, `h + 1`
  darbesi B'den uzak etki olarak `h + 2`'de).
- *Kristalleşme göçü* (mover `h`'de partnerin shard'ına): partnerin `h`
  darbesi C1 yönlendirmesiyle (§4c madde 7, değişmedi); eski shard'da
  kopyaya yerel vuran üçüncü biri artık yeni sahibe gider.
- *Bırakma / bant / partner göçü:* tutulan çiftin partneri `h + 1`'de
  YEREL vurur — D'den önce kayıp, şimdi yeni sahipte bir kez (MMO testi).

**Elenen alternatifler.**

1. *Yalnız `Seam`'i yeniden etiketlemek* (kopya dünyada görünür kalır;
   `local` = `None`, `lent` + `emit` yeni sahibe): oyunun her dünya-sorgusu
   yolu (MMO melee, alan etkisi) kopyayı vurmaya devam eder, "dünya ∪
   ödünç" onu iki kez gösterir, kopyanın sistemleri ikinci kez oynar —
   her oyun her sorgusunu `seam.local` ile süzmek zorunda kalırdı.
2. *Kopyayı erken despawn* (kit `h + 1`'in ilk kancasında `on_migrate_out`
   çağırır, ya da çekirdek 4a'yı faz 0'a taşır): yapısal olarak güçlü ama
   `h + 1` border export'undan e'yi düşürür (üçüncü shard onu bir tick
   erken kaybeder), spatial kompozitin çıkış (`pending_removals`)
   zamanlamasını kaydırır; çekirdek varyantı göç protokolünün faz
   sırasını değiştirir — kaçınılabilir çekirdek değişikliği.
3. *Capture'ı taşımak* (`h + 1`'in 4a'sında yakala): Migrate `h`'de gitti;
   geç capture gönderimi bir tick geciktirmek, yani tek-tick hizalamasını
   iki tick'e çıkaran protokol değişikliği demek.
4. *Kopyaya yazılanı sonradan yeni sahibe aktarmak* (fark): kit oyun
   durumunu bilmez; genel bir fark yok.
5. *Çekirdeksiz commit tahmini* (kit 4b'de raporladığını gitti sayar):
   reddedilen gönderimde (dolu komşu kutusu) e A'da kalır; kit onu gizleyip
   darbeyi B'ye yollarsa (B'de yok → `NoTarget`) hem darbe hem bir tick
   hareket kaybolur.
6. *Çekirdek `emit`'te önce yönlendirme, sonra ödünç*: pencere dışında
   (e B'den D'ye geçmişse) bir fazla atlama; "ödünç önce" korundu,
   yönlendirme yalnız kimse ödünç vermiyorsa.

**Bedel ve sınırlar.**

- `Disabled` sorguları süzer, DOĞRUDAN erişimi değil: oyunun sakladığı bir
  `Entity` tutamağıyla (`world.get_mut(e)`) kopyaya yazan kod onu hâlâ
  yazar. Sözleşme: hedef wire'dan (sorgu ya da `Seam::local`) çözülür —
  MMO böyle yapar.
- Kopya `h + 1`'de donuk: kit geçişleri (o tick'in border export'u) onu
  yakalanan değerle görür; eskiden kopyanın sistemleri onu bir tick daha
  ilerletiyordu (kimsenin okumadığı hayalet değer — B sahip-kazanır ile
  kendi kaydını kullanır; üçüncü shard bir tick için bir tick daha bayat
  değer görür). Hareket etmeyen fikstürde `h + 1` bayt-bayt aynı (test).
- Nadir köşe: e A'ya az önce başka bir komşu C'den geldiyse C'nin bayat
  ödünç kaydı `h + 1`'de hâlâ durabilir; oyuna görünen kiralayan B'dir ama
  çekirdek "ödünç önce" kuralıyla C'ye yollar → C'nin yönlendirmesi → A
  → B (2 atlama ≤ 3): doğru, bir tick geç.
- Maliyet: göç başına bir kayıt kopyası ve iki arketip taşıması; göç
  yokken bir `is_empty`.

**Testler.** Çekirdek (`effects/flow.rs`, 1): `h + 1`'de kimsenin ödünç
vermediği ayrılmış hedefe `emit` yeni sahibe, sıradan damgayla gider,
`departed` onu adlandırır; TTL sonrası `NotLent`. Kit (`sharded/tests/
departing{.rs,/brawl.rs}`, 4; fikstür `Brawling`: can, betikli ve otomatik
saldırı, MMO gibi dünya-sorgulu yerel darbe): (1) yerel darbe kopyaya değil
yeni sahibe, bir kez (100 → 90 yerel, 90 → 80 B'de); 0d'deki başka bir
etkinin kancası da kopyayı görmez; B'nin erken export'u görünümü
değiştirmez; kopya sistemlerden sonra geri ve export'ta; kontak seam ötesi
çift; migrate-out kaydı unutur. (2) Reddedilen gönderim: yerel kalır,
yerel vurulur, kayıt unutulur. (3) Kopya ikinci kez davranmaz: otomatik
saldırı yalnız B'den, bot yalnız B'de. (4) Dokunmayan oyun değişmez:
commit'li ve commit'siz koşu aynı dünya, export, snapshot ve göç. MMO
(gerçek dört shard, `migration_tick.rs`, 2): bölge geçişi düellosu (P ve Q
`h` ve `h + 1`'de karşılıklı; P'nin 4. darbesi Q'yu shard 1'de `h + 2`'de
yener, kredi P'ye; her darbe tam bir darbe hasarı, hasar toplamı = kayıp
can) ve kristal bırakma (P'nin `out + 1` darbesi Q'yu shard 1'de
`out + 2`'de yener).

Önce-kırmızı: iki MMO testi düzeltmeden önce (darbe `(0, h + 1)`
kopyaya); çekirdek testi `NotLent`; kit yarısı kapalıyken 2 kit + 2 MMO
testi kırılır (kalan iki kit testi "hiçbir şey değişmez" iddiasıdır).
Mutasyonlar (hepsi yakalandı): kit 16 — gizleme yok; gizleme commit'e
bakmaz; sistemlerden sonra gösterme yok; `local` kopyayı döndürür; `lent`
ayrılan kaydı vermez; `lent` erken export'u tercih eder; `lent_iter`
ayrılanın ödünç kopyalarını tutar; `lent_iter` ayrılanı vermez; `emit`
ayrılanı `Local` sayar; kontak kopyayı yerel sayar; bot kopyayı sürer;
capture kayıt tutmaz; migrate-out unutmaz; 0d'de / ingest'te / step'te
gizleme yok — çekirdek 2 — `emit` yönlendirmeye düşmez; `departed` hiçbir
şey demez.

**Yük sağlaması** (release, süreç içi, 32 çekirdek, başka bir ajanın
derlemesiyle paylaşılan makine — koşu öncesi 1 dk yük 4,8–5,5; sayılar
kıyas değil sağlamadır): `gsb-loadgen 200 --game mmo --duration 20
--write-stall-secs 0` → `joined = left = 200`, `errors=0`,
`server_closes=0`, `server_hz` 30,00, adım p50/p90 fine 128/176 µs,
`out_bps_per_conn` 20 683. Düello kipi (`--mmo-duel-frac 0.2 --duration
90`, C2'nin komutu) → aynı üç sıfır, 30,00 Hz, adım p50/p90 168/224 µs,
`out_bps_per_conn` 16 143; dört shard'ın `remote_effect_summary`'si
161 uygulanan uzak etki (~1,8/sn, C2 tablosuyla aynı düzey), 0 yetim,
0 düşen, 2 politika reddi; 23 `crystal_move`, 46 `crystal_release`.

**Göç sayısı raporu (küçük paket, BACKLOG F3).** Çekirdek artık göçü
sayıyor — shard aktörü, `RoomCounters` üzerinden her örnekte:
`migrations_out` (gönderimi kesinleşen `Migrate`), `migrations_in`
(kurulum kapısından geçip kurulan) ve `migrations_failed` (dolu komşu
gelen kutusunun reddettiği gönderim — entity kalır, satır geri alınır,
geçiş sonraki tick yeniden denenir). Tek oda aktöründe üçü de 0.
Katlamada SUM; bir göç kaynağında `out`, hedefinde `in` olarak bir kez
sayılır, yani katlanmış ikili eşit çıkmalı (ikisi toplanmaz). Epoch
kapısının düşürdüğü hayalet `Migrate` (ayrılma önce işlendi) sayılmaz —
debug satırı olarak kalır (nadir, oturum ölümüyle yarışın izi, yük
sinyali değil). Prometheus: `gsb_room_migrations_{out,in,failed}_total`
(OPS §3).

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

### 6.2 Dış öneri (~~kabul: değerlendiriliyor~~ değerlendirildi — §7: A/B'de kabul, bugün süreç içinde uykuda)

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

### Karar (A/B ölçümü sonrası güncellendi): delta ~~KABUL EDİLDİ ✅~~ kabul edildi, bugün 💤 uykuda (süreç içi link'ler `AlwaysFull` — aşağıdaki "Güncel durum")

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

## 8. team × sharded kompoziti: registry-hub takım-export (~~tasarım hazır~~ tasarım — W1'de uygulandı, sapmalarıyla §8b)

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
- v1 dışı: ~~cross-seam ETKİLEŞİM (sadece görünürlük)~~ *(kapandı —
  seam ötesi etkileşim C1/C2'de, §4b/§4c; W2 onu team × sharded odada
  kullanıyor, §8b.8. Takım export'unun kendisi hâlâ yalnız görünürlük
  taşır: etki yalnız ödünç kayda gider, ithal kayda değil)*; otomatik
  balancer; kalıcılık entegrasyonu.

## 8b. W1 sonucu — team × sharded kompoziti (branch `kit/w1-team-sharded`)

> Durum: **uygulandı** (W1, §8b.7). §8b.1–§8b.6 kod yazılmadan önce
> tasarım olarak yazıldı; §8 sözleşmedir, aşağıdaki her sapma
> gerekçesiyle kayıtlıdır (uygulamada değişen tek kural — nötrler —
> §8b.5 madde 8'de işaretli). BACKLOG §1 satır 6'nın (W) ilk yarısı: W1
> kompozit, W2 onun üstüne doğrulama oyununu kurar.

**Senaryo ("Cephe", W2'nin oyunu).** Büyük harita 2×2 shard ızgarası
(MMO gibi zemin düzlemi bölümü), üç fraksiyon, TAKIM SİSİ: oyuncu (a)
kendi fraksiyonunun HER birimini harita genelinde görür (minimap / parti
çerçeveleri — hangi shard'da olursa olsun), (b) bir düşmanı YALNIZ
fraksiyonundan bir birim onu görüyorsa görür (zemin düzleminde görüş
yarıçapı) — gören müttefik, düşman ya da ikisi birden oyuncudan başka bir
shard'da olsa bile (shard 0'daki bir müttefik gözcü kulesi shard 0'daki
bir düşmanı görür; shard 3'teki fraksiyon oyuncusu o düşmanı görür).
Border şeridi (lokalite) bunu veremez: lokalite-karşıtı ilgi.

### 8b.1 Karar: üyeler DEĞİL, takımın GÖRÜNÜR KÜMESİ export edilir

§8 yalnız takım ÜYELERİNİ export ediyordu; bu (a)'yı verir, (b)'yi
vermez: shard 0'daki düşmanı gören shard 0'daki müttefiktir, shard 3
o düşmanı hiçbir yoldan öğrenemez. **Karar: bir shard her takım T için
T'nin BU shard'daki görünür kümesini export eder** =

- T'nin bu shard'daki üyeleri (oyuncu birimleri VE `TeamMember`
  taşıyan NPC'ler — gözcü kuleleri),
- bu shard'ın T birimlerinin gördüğü düşmanlar: bu shard'ın KENDİ
  entity'leri ve border şeridinden ÖDÜNÇ aldığı kayıtlar (seam'in
  ötesinde duran düşman: gören müttefik bu shard'da, düşman komşuda).

Hiçbir zaman export EDİLMEYEN: ithal edilmiş kayıtlar (yankı döngüsü
olurdu — kayıt yalnız onu yerel olarak bilen shard'dan çıkar) ve
kimsenin görmediği nötr entity'ler (§8b.5 madde 8).

**Sınırlar (sayılan, asla sınırsız değil).**

- Kit, takım başına tick başına bütçe: `with_team_budget(n)` (varsayılan
  `DEFAULT_TEAM_BUDGET` = 1024 kayıt). Aşımda önce üyeler, sonra görülen
  düşmanlar; fazlası kesilir ve sayılır. *A29:* her kademenin İÇİNDE ne
  kalacağını oyun seçebilir — `with_export_rank(fn(&Wire<G>) -> u32)`:
  yüksek sıra önce, eşitlikte küçük wire id (şeridin sırasından
  bağımsız); sıralama üyeler-önce kuralının yerine geçmez, onu inceltir
  (üyelik yalnız kit'te bilinir; ödünç kaydın takımı yok). Sıralama
  yoksa kademe içinde kit'in sırası (kendi entity'ler wire id'ye göre,
  sonra şerit) — A29 öncesiyle aynı. Kesme yalnız bütçe aşılınca, kesilen
  kademede doğrusal seçimle (takım başına O(n), sort yok, tahsis yok);
  kalanlar export sırasında gider. Kesilen kayıt export'la çekirdeğe
  raporlanır (`TeamExport::over_budget`, röle edilmez) →
  `RoomSample::team_over_budget` (KIT-ARCHITECTURE §10 "A29").
- Çekirdek, mesaj başına sert tavan: `TEAM_EXPORT_MAX_RECORDS` (16 384
  kayıt) ve `TEAM_EXPORT_MAX_VIEWS` (256 takım); aşan kesilir, shard'ın
  `over_cap` sayacı artar. Bir oyunun bütçesi ne olursa olsun registry
  ve alıcılar bu tavanla sınırlı.
- Alıcı: kaynak shard başına bir yuva, yuva ≤ `TEAM_EXPORT_MAX_RECORDS`
  → shard başına ≤ (N − 1) × tavan.

**Bedel (§8.4'e karşı).** §8.4 export'u O(kendi takım üyeleri) diye
sınırlıyordu; görünür küme O(kendi birimleri × onları gören takım
sayısı) — bir birim en fazla takım sayısı kadar kez export edilir (3
fraksiyonda ≤ 3×). Kodlama birim başına önbellekli: kayıt baytları wire
değeri değişmedikçe yeniden kodlanmaz (`Bytes` refcount'la paylaşılır).
Yayılım: bir kayıt, takımını görüntüleyen her DİĞER shard'a gider (≤ N −
1). 4 shard, 1000 birim, üç fraksiyon: tick başına kabaca 1000 üye
kaydı + görülen düşmanlar, her biri ≤ 3 hedefe — `Bytes` klonu, kopya
yok. Bu, senaryonun doğasıdır: "müttefikler harita genelinde" = her
shard, görüntülediği takımların bütün birimlerini her tick bilir. Bu
maliyet registry'nin TEK görevine veri düzlemi yükü bindirir (§8 bunu
kabul etti); W2'nin ölçümü tetikleyici olursa A13 (registry striping)
ya da export temposu (`every k ticks`) açılır — W1'de ikisi de yok.

### 8b.2 Hub: registry'nin oda girdisinde, kaydı saklamayan röle

- **Mesaj (monomorfik, byte-encoded).** Shard → registry:
  `RegistryMsg::TeamExport { room, generation, from, tick, export:
  TeamExport { views: Vec<u64>, records: Vec<TeamRecord> } }`,
  `TeamRecord { team: u64, wire: u64, bytes: Bytes }` — `bytes` oyunun
  `RecordCodec::encode` gövdesi, registry içeriğe hiç bakmaz.
  `views` = bu shard'ın OYUNCULARININ takımları (görüntüleyici
  abonelikleri). Registry → shard: `ShardMsg::TeamImport(TeamImport {
  from, tick, records })`. `RegistryMsg` monomorfik kalır (generic'e
  çevirmek §8.1'de ELENDİ; burada da gerek yok).
- **Tablo.** Oda başına, registry'nin oda girdisinin içinde
  (`ShardGroup::teams: TeamHub`): kaynak shard başına bir yuva `{ tick,
  views, relayed: hedef başına "boş olmayan ithal tutuyor" bayrağı }`.
  **Kayıt saklanmaz.**
- **Röle (export başına).** `from`'un yuvası wholesale değişir; sonra
  DİĞER her shard t için (t'nin yuvası varsa): `from`'un kayıtlarından
  takımı t'nin `views`'ünde olanlar süzülür; boş değilse ya da t
  `from`'dan boş olmayan bir ithal tutuyorsa (`relayed[t]` — onu
  temizleyen tek boş mesaj) `try_send`. Başarıda `relayed[t]` güncellenir;
  düşmede (dolu/kapalı posta kutusu) sayılır ve bayrak kalır — sonraki
  export tekrar dener. Kendine röle yok; görüntüleyicisi olmayan (yalnız
  gözcü kulesi barındıran) shard'a röle yok.
- **İzolasyon.** Süzgeç = hedefin görüntülediği takımlar: A'nın kaydı
  A'yı görüntülemeyen shard'a hiç gitmez; aynı shard'daki B
  görüntüleyicisine sızmaz çünkü alıcı logic takım T'nin içeriğine
  YALNIZ `imports.team(T)`'yi katar (test kilidi, iki katmanda).
- **Süpürme.** `TEAM_EXPORT_TTL_TICKS` (= 64) boyunca export etmeyen
  kaynağın yuvası (abonelikleri) düşer; süpürme oda başına
  `TEAM_HUB_SWEEP_EVERY_TICKS` (= 64) tick'te bir, export'ların
  tick'iyle (registry'nin saati yok) — O(N) retain, amortize.
- **Nesil.** Export `generation` taşır (shard'ın kurulum nesli, uzak
  etkilerin epoch'u); oda girdisinin nesli tutmayan export (ölmüş bir
  enkarnasyonun geç mesajı) düşer. Oda yok edilince / ölünce hub girdiyle
  birlikte gider — ayrı temizlik yok.

### 8b.3 Alıcı taraf

- CONTROL'de `TeamImport` kaynağın yuvasını **wholesale** değiştirir
  (`TeamImports`, çekirdek). Her tick: `TEAM_EXPORT_TTL_TICKS`'ten eski
  yuva düşer (sessizleşmiş kaynak — hayalet kalıcılaşmaz), sonra
  takım başına birleşik görünüm kurulur: aynı wire birden çok kaynaktan
  gelirse (göç geçişi) en yeni `tick` kazanır, eşitlikte küçük kaynak
  indeksi; wire'a göre sıralı.
- **Yeni faz 5b — TEAMS** (BORDER'dan sonra, BROADCAST'tan önce):
  ödünç küme (faz 6'nın zaten kurduğu düzleştirilmiş, own-wins süzülmüş
  dilim — artık bir kez kurulup iki faza veriliyor) ve ithal küme
  logic'e verilir, logic bu tick'in export'unu döndürür, çekirdek onu
  (tavan uygulanmış) registry'ye `try_send` eder. Export boşsa ve bir
  önceki gönderilen de boşsa mesaj yok; boş olmayan bir export'tan sonra
  bir kez boş gider (temizleme) — düşerse sonraki tick tekrar.
- **Seam (en küçük ek).** `GameLogic` DEĞİŞMEZ. `ShardLogic` bir kanca
  kazanır (varsayılan `None` — bugünkü her shard logic'i hiçbir şey
  göndermez, hiçbir şey almaz):

  ```
  fn team_exchange(&mut self, world: &mut W, ctx: &TickCtx,
                   borrowed: &[BorderRecord<Self::Strip>],
                   imported: &TeamImports) -> Option<TeamExport>
  ```

### 8b.4 Kit: `ShardedTeamRoom<G, P, V>`

`ShardedSpatialRoom` kalıbı: grid protokolü (göç, ödünç, seam, park,
RPC) sarılan `ShardedRoom<G, P>`'de; kompozit takım yüzeyini ekler —
`GroupKey = Team`, `V: Vision` görüşü, `with_delta` (ortak
`common::SetLedger` + `Baselines`), takım bütçesi. `G: ShardGame +
TeamGame`.

- **Birleştirme ve öncelik.** Takım T'nin içeriği (her tick, `team_exchange`
  içinde): kendi T üyeleri; kendi nötr entity'leri (TeamRoom kuralı —
  bu shard'daki herkese); bir kendi T biriminin gördüğü kendi düşmanları
  ve ödünç kayıtları; `imports.team(T)`. Aynı wire bir kez; YÜK önceliği
  **yerel > ödünç > ithal** (ithal edilen bir wire burada kendi ya da
  ödünç kaydıyla da biliniyorsa taze tipli değer gösterilir), GÖRÜNÜRLÜK
  birleşim (ithal kayıt = "bu wire T'ye görünür"). Seam'e yakın, seam'in
  ötesindeki müttefikçe görülen düşman: komşu onu ödünç kaydı olarak
  görür ve export eder; burada kendi entity'miz → tek kayıt, yerel yük —
  çift teslim yok.
- **Ödünç kaydın görüş konumu.** Ödünç kayıt yalnız wire değeri taşır
  (`Strip = Wire` — C1'in seam kancaları bu tipe bağlı, değiştirilmez);
  görüş testi simülasyon konumu ister. Kompozit kurucusu
  `lent_pos: fn(&Wire<G>) -> Option<V::Pos>` alır (oyun: nicemlenmiş
  wire'dan konum; `None` = ödünç kayıt görüşe katılmaz, yalnız ithalle
  görünür). Ödünç kaydın TAKIMI bilinmez: ödünç kayıtlar görüş KAYNAĞI
  değil yalnız HEDEF — seam'in ötesindeki müttefiğin görüşünü onun kendi
  shard'ı hesaplar ve export eder (simetri).
- **Kodlama.** İçerik değeri `Shown<W> { Typed(W), Encoded(Bytes) }`:
  kendi/ödünç kayıt tipli değer, ithal kayıt kodlanmış gövde.
  `SetLedger` yazım sınırını `RecordCodec`'ten crate-özel bir
  `WriteRecord<W>` trait'ine gevşetir (`RecordCodec` için blanket impl —
  mevcut odalar bayt bayt aynı); kompozitin yazıcısı `Encoded`'ı olduğu
  gibi `entities` zarfına koyar.
- **Takım göçte taşınır.** `TeamMember` kit'in bileşeni, oyunun
  `capture`'ı onu bilmez: kompozitin göç durumu `TeamMig<M> { kit:
  KitMig<M>, team: Option<Team> }`, `on_migrate_in` bileşeni yeniden
  yazar. Göçle gelen oyuncunun oturumu yeni shard'ın takım görünümüne
  baseline'sızdır → delta modunda one-shot private full (spatial
  kompozitin taze-üye kuralı).
- **K4 kalıntısı.** `TeamGame::spawn_team_player_as(world, conn,
  identity)` — varsayılan `spawn_team_player`'a delege (arena'nın üs
  doğumu değişmez); `TeamRoom` ve kompozit kimlikle çağırır.

### 8b.5 Kararlar ve §8'den sapmalar (gerekçeli)

1. **Görünür küme, üye değil** (§8b.1) — senaryonun (b)'si.
2. **Hub kayıt SAKLAMAZ; export başına röle.** §8.2 tabloda kayıt tutup
   agregayı yayıyordu. Agrega her export'ta yeniden kurulacaktı ve
   yayılım temposu export temposuna eşit; taze export'u doğrudan rölelemek
   aynı yayılım, yarı bellek. Hub'ın geri alamadığı (gönderilmiş) kayıt
   yüzünden alıcı TTL'i zaten şart — kaynak-başına yuvalar alıcıda.
3. **Wholesale değişim (oda, kaynak shard) düzeyinde**, (oda, takım,
   kaynak) değil: shard tick başına TEK export'la bütün takımlarını
   taşır; yeni export'ta olmayan takım yok demektir → ayrılan üye bir
   SONRAKİ export'ta kaybolur (TTL'i beklemez). TTL yalnız export etmeyi
   bırakan kaynağın (ölen, ya da son temizleme mesajı düşen) sigortası.
4. **Tablo registry'nin oda girdisinde** (`ShardGroup`), `(RoomId,
   takım)` anahtarlı ayrı harita değil: odalar arası izolasyon ve
   destroy/ölüm temizliği yapısal.
5. **`GameLogic` ikinci dilim YOK; `ShardLogic::team_exchange`.** §8.3
   `snapshot`'a ikinci dilim diyordu: ~40 uygulama ve ~50 çağrı yeri
   dalgalanırdı, tek oda aktörü asla ithal görmez. Export görüş-bilinçli
   (ödünç düşmanlar dahil) olduğundan ödünç + ithal kümeyi okumak ve
   export'u üretmek tek çağrı; snapshot çağrıları logic'in bu tick
   kurduğu takım içeriğinden okur (TeamRoom'un `update` önbelleği gibi).
   `ingest_seam`/`update_seam` emsali: varsayılanlı shard-alt-trait
   kancası.
6. **Mesaj alanları.** Tuple yerine adlı `TeamRecord`; `views`
   (yayılımın "kim görüntülüyor" bilgisi, §8.2 bunu "export gönderen
   shard kümesi"nden türetiyordu — gözcü kulesi barındıran ama
   görüntüleyicisi olmayan shard gereksiz röle alırdı), `tick` (TTL ve
   dedup), `generation` (ölü enkarnasyon).
7. **TTL 64 tick** (§8.2'nin örneği 256): canlı kaynak her tick tazeler;
   TTL yalnız sessizleşmiş kaynağın hayaletinin ömrünü sınırlar — 30
   Hz'de ~2 s.
8. **Nötrler: kendi shard'ında herkese, başka yerde sisle.** Kendi
   shard'ında her takıma (TeamRoom kuralı); başka shard'daki bir takım
   onu yalnız bir birimi görürse görür — düşman gibi. *Uygulamada
   değişti:* tasarım "nötrler export edilmez" diyordu; ama ödünç bir
   kayıt nötr/düşman ayırt edilemez (takımı yok), yani komşunun gördüğü
   ödünç nötr zaten export ediliyordu — kendi nötrünü gören birimin
   export'undan çıkarmak kuralı yerine göre değiştirirdi. Tek kural:
   bir takımın birimlerinin gördüğü, üyesi olmayan her şey (düşman ya da
   nötr, kendi ya da ödünç) export edilir. Harita geneli nötr (ele
   geçirme noktası) W2'nin kararı.
9. **Göç: kopya yok, en fazla bir tick boşluk.** Üye m, A → B, tick h:
   B'nin görüntüleyicileri m'yi `h + 1`'de kendi kaydı olarak görür;
   üçüncü shard C, A'nın `h` export'undan (`h + 1`) sonra B'nin `h + 1`
   export'undan (`h + 2`) görür — yuvalar arası dedup wire'la, kopya
   yok. A'nın görüntüleyicileri `h + 1`'de m'yi kaybeder (A onu despawn
   etti, B'nin `h` export'unda henüz yok), `h + 2`'de ithalle geri alır:
   bir tick boşluk (kabul).

### 8b.6 Sayaçlar: metrik yolu değil, log satırı

> *W2'de terfi etti (A26): shard sayaçları kümülatif, `RoomSample`'ın
> `team_*` alanlarında; log satırı pencereyi tutuyor — §8b.8. A29'da
> kit'in bütçe kesmesi de (`over_budget`) aynı yoldan.*

Shard penceresi (`TeamStats`, ~1 s): `exports`, `export_drops`,
`export_records`, `over_cap`, `imports`, `import_records`, `expired` —
sıfır değilse `team_exchange_summary` info satırı (§7'nin
`border_exchange_summary` emsali). Hub: `exports`, `relays`,
`relay_drops`, `relay_records`, `expired` — oda başına 256 export
tick'inde bir `team_hub_summary`. Gerekçe: `RoomSample` sabit şekilli
ve loadgen'in metrik tel formatına (GSM9) ve Prometheus'a bağlı; küçük
paket turu dokuz sayaç için 29 dosyaya dokundu. Henüz üretim kullanıcısı
olmayan bir özellik için tel sürümü değiştirmek erken; loadgen botunu
kuracak W2, ölçüm isterse terfi ettirir.

### 8b.7 Uygulama (W1 — `kit/w1-team-sharded`, `e99d90c..`)

**Commit'ler** (her biri kendi başına yeşil): `5c73895` (bu tasarım),
`405e898` (çekirdek: TEAMS fazı + registry hub'ı), `83f040b` (kit:
`TeamGame::spawn_team_player_as`, K4 kalıntısı), `2fb51bd` (hub'ın
kaynağa geri röle etmediğini kilitleyen test), `5a47687` (kit:
`ShardedTeamRoom`), `4d1d231` (gerçek registry + dört shard aktörüyle
senaryo testleri).

**Çekirdek seam farkı (tamamı).**

- `ShardLogic::team_exchange(&mut self, world, ctx, borrowed, imported)
  -> Option<TeamExport>` — varsayılan `None`. `GameLogic` DEĞİŞMEDİ.
- Yeni tipler (`gsb_core::shard`): `TeamRecord { team, wire, bytes }`,
  `TeamExport { views, records }`, `TeamImport { from, tick, records }`,
  `TeamImports` (+ `ImportedRecord`); sabitler `TEAM_EXPORT_TTL_TICKS`
  (64), `TEAM_HUB_SWEEP_EVERY_TICKS` (64), `TEAM_EXPORT_MAX_RECORDS`
  (16 384), `TEAM_EXPORT_MAX_VIEWS` (256).
- `ShardMsg::TeamImport(TeamImport)`; `RegistryMsg::TeamExport { room,
  generation, from, tick, export }`. İkisi de monomorfik içerik.
- Faz 5b TEAMS (`shard/actor/tick/teams.rs`): süpür + birleştir → kanca
  → tavan → `try_send`. Düzleştirilmiş ödünç küme artık bir kez
  (`borrowed_view`) kurulup faz 5b'ye ve 6'ya veriliyor — faz 6'nın
  davranışı aynı.
- Registry: `ShardGroup::teams: TeamHub` (`registry/hub.rs`), `run.rs`'te
  bir kol (nesil denetimi). Hub yalnız abonelik tutar, kayıt tutmaz.
- Sayaçlar: `team_exchange_summary` (shard, ~1 s) ve
  `team_hub_summary` (oda, 256 export tick'i) log satırları.

**Kit.** `sharded/team.rs` (+ `content`, `frames`, `logic`, `shard`):
`ShardedTeamRoom<G, P, V>` (`with_shard(inner, vision, lent_pos)`,
`with_delta`, `with_team_budget`, `with_crystallize`,
`with_disconnect_*`, `over_budget`), `TeamMig<M>`,
`DEFAULT_TEAM_BUDGET`. `SetLedger`'ın yazıcısı crate-özel
`WriteRecord<W>`'ye gevşedi (`RecordCodec` için blanket impl — mevcut
odaların baytları aynı; `team/tests/delta/full_only` kilidi yeşil).
`ShardedRoom::admit` (katılım yardımcısı, `room/join.rs`) iki oda
arasında paylaşılıyor. `Team` artık `Ord`. Kit'e dev-dependency olarak
`tokio` (`test-util`): gerçek aktör testleri duraklatılmış saatte
koşuyor — ticker yalnız bütün aktörler boştayken ilerler, yani bir
tick'in export'ları bir sonraki tick başlamadan rölelenmiş olur ve
adım bariyeri kesin.

**Testler** (740 → 776, +36). Çekirdek 17: `shard/team/tests.rs` (6:
wholesale, takım izolasyonu, en yeni tick kazanır / eşitlikte küçük
kaynak, TTL, eski import yeni yuvayı ezmez, yuva tavanı),
`registry/hub/tests.rs` (6: yalnız o takımı görüntüleyen DİĞER
shard'lara, hiç export etmemiş shard'a hiçbir şey, bir kez temizleme
sonra sessizlik, reddedilen röle sayılır ve tekrar denenir, TTL
süpürmesi, bilinmeyen indeks), `shard/tests/teams.rs` (5: export
registry'ye damgalı gider / `None` ve registry'siz shard göndermez, bir
kez boş export, reddedilen export sayılır ve temizleme tekrar denenir,
tavanlar, import mantığa birleşik ulaşır ve TTL'de düşer). Kit 19:
K4 2 (`team/tests/spawn_team.rs`), kompozit birim 9
(`sharded/tests/team*`: üyeler önce + görülenler, bütçe, nötr kendi
shard'ında herkese, export gövdesi birimi izler ve değişmedikçe aynı
tahsis, bir wire bir kez + öncelik yerel > ödünç > ithal, ithal asla
yeniden export edilmez, ithalde takım izolasyonu, ödünç kaydın görüşü
ve `lent_pos = None`, göçte takım taşınır, gelen oyuncuya one-shot
full, giden-dönen oyuncuya yeniden one-shot full), gerçek aktör 8
(`sharded/tests/team_actors*`: müttefik harita geneli + uzak müttefiğin
gördüğü düşman — full ve delta modda; üçüncü takımın tüm koşu boyunca
izolasyonu; seam ötesi düşman tek kayıt, kendi shard'ının taze
kaydıyla; ayrılan üye bir sonraki export'la gider, TTL sonrası da
hayalet yok; sessizleşen kaynağın kaydı TTL'de düşer (63. tick'te var,
65.'te yok); bayat küme bir sonraki export'la iyileşir; başka
enkarnasyonun export'u yok sayılır; seam'i yürüyerek geçen üye hiçbir
karede iki kez yok, hiçbir görüntüleyicide bir tick'ten uzun
kaybolmuyor — ayrıldığı shard'daki müttefikte tam bir tick, varış ve
uzak shard'da sıfır).

**Önce kırılan.** K4: `TeamRoom` kimliği iletmezken (turdan önceki
`logic.rs`) yeni test kırıldı. Kompozit: `team_exchange` `None`
döndürünce (varsayılan kanca — takas yok) ve çekirdekte TEAMS fazı hiç
koşmayınca, 8 gerçek aktör testinin 8'i de kırıldı.

**Mutation-check** (her biri scratchpad yedeğinden geri yüklendi;
hepsi en az bir testi kırdı): hub her takımı her hedefe röle ediyor
(izolasyon); hub kaynağın yuvasını röle sırasında tabloda tutuyor
(kendine röle); hub görüntüleyicisi olmayan shard'a boş import
gönderiyor (yalnız barındıran shard'lara yayılım); temizleme importu
yok; hub süpürmesi hiç düşürmüyor; alıcı yuvaları hiç süresi dolmuyor
(TTL — çekirdek 2 + gerçek aktör 1); birleştirmede en eski tick
kazanıyor; dedup yok; yuva wholesale değil (ekleme — gerçek aktörde
bayat küme ve ayrılan üye testleri de kırıldı); yuva tavanı yok; export
tavanı yok; temizleme export'u yok; hiçbir şey tutulmazken de her tick
gönderim; reddedilen export yine de "tutuluyor" sayıyor; `settle` yok
(9 test); registry nesil denetimi yok. Kit: her takımın ithali her
takıma (izolasyon — birim 1 + gerçek aktör 2); ithal bilinen değeri
eziyor ve bilinen wire ithal baytından gösteriliyor (öncelik — birim +
seam testi); bütçe yok sayılıyor; bütçe üyeleri kesiyor; düşman görüşsüz
export; ödünç kayıt hiç görülmüyor; ithal yeniden export; görüntülenen
takımlar = birimi olan her takım; kendi nötrü gösterilmiyor; gövde
önbelleği hiç yeniden kodlamıyor / hiç yeniden kullanmıyor; göçte takım
taşınmıyor; göç çıkışı VE girişi baseline'ı silmiyor (ikisi birlikte:
biri diğerinin yedeği — ikisinden biri tek başına eşdeğer mutant);
katılımda kimlik düşüyor; ithal gövdesi zarfsız yazılıyor.

**Ölçüm — mevcut sharded oyunlar etkilenmedi** (release, 200 istemci,
`--duration 10 --write-stall-secs 0`, `e99d90c` ↔ W1 dönüşümlü üç çift;
makine başka ajanların derlemeleriyle yüklüydü — 1 dk yük ortalaması
42–54 / 32 çekirdek). Her koşuda `left=200 errors=0 server_closes=0`,
`server_hz` 29,98–30,00:

| Senaryo | Yük (taban / W1) | `out_bps_per_conn` taban / W1 | step p50/p90 fine µs taban / W1 | peak payload B taban / W1 |
|---|---|---|---|---|
| MMO (`--game mmo`) | 46,5·45,4·46,3 / 53,6·48,1·46,4 | 21 621·21 720·21 165 / 21 510·21 204·21 525 | 224/760·200/384·224/664 / 224/736·208/312·208/568 | 911·900·900 / 876·895·893 |
| demo sharded (`--visibility sharded --shard-count 4`) | 51,2·46,1·45,0 / 50,0·44,1·42,2 | 28 569·28 570·29 350 / 28 535·28 878·29 470 | 184/352·184/320·160/224 / 176/288·192/576·160/256 | 1504·1500·1517 / 1534·1480·1528 |

İki oyunun da mantığı `team_exchange`'i uygulamıyor (varsayılan `None`):
faz 5b onlar için bir birleştirme çağrısı (boş) ve bir sanal çağrıdan
ibaret; fark gürültü içinde. MMO'nun `gap_drops`'u (72/148/152) ve
demo'nun `snap_overflows`'u (24–49) iki tarafta da aynı aralıkta.

**Kapılar** (kod commit'lendikten sonra, her crate'in `lib.rs`'ine
yeniden derleme işareti eklenerek, sonra `git checkout crates`):
`cargo fmt --all --check` temiz; `cargo clippy --workspace
--all-targets -- -D warnings` 0 uyarı; `cargo test --workspace` →
**776 passed / 0 failed / 1 ignored**; kapanış kontrolü `cargo test -p
gsb-demo -p gsb-demo-arena -p gsb-demo-mmo` → **98 passed / 0
failed**; `cargo build -p gsb-server --lib --no-default-features`, ve
ayrı ayrı `--features game-demo`, `--features game-arena`, `--features
game-mmo` temiz; `RUSTDOCFLAGS="-D warnings" cargo doc --workspace
--no-deps` temiz. Byte-pinning testleri dokunulmadı ve yeşil (mevcut
hiçbir oda/oyunun istemci baytı değişmedi).

**W2 için: bir oyun bunu nasıl kullanır.**

1. Oyun `ShardGame + TeamGame` uygular; saklı karakteri
   `spawn_team_player_as(world, conn, identity)` ile yerleştirir (takım +
   konum), sunucu modülünün `home_shard`'ı aynı kimlikten aynı konumu
   okur. Gözcü kulesi/ward = `TeamMember` + konum bileşeni taşıyan
   NPC (kit orphan olarak damgalar, görüş kaynağıdır).
2. Fabrika her shard için `ShardedTeamRoom::with_shard(ShardedRoom::
   with_game(game, partition, i), VisionGrid2::new(r), lent_pos)` kurar;
   `lent_pos` nicemlenmiş wire'dan görüş konumu (`None` = ödünç kayıt
   görüşe katılmaz). `with_delta()` (arena gibi), gerekiyorsa
   `with_team_budget(n)`.
3. İstemci tarafı değişmez: kare zarfı takım odasınınki (`ClientView`,
   `kit.proto`). İthal kayıt, kaynağın kodlayıcısının yazdığı gövdedir —
   istemci ayırt edemez.
4. W2'nin açık kararları: harita geneli nötrler (ele geçirme noktası —
   bugün kendi shard'ında herkese, başka yerde sisle), export temposu
   (bugün her tick; `every k` gerekirse), sayaçların metrik yoluna
   terfisi (loadgen botu kurulunca), registry bandı (A13 tetikleyicisi —
   loadgen'le ölçülecek), `Private.game` ile takım bildirimi (arena'nın
   `Welcome` kalıbı).
   *W2'de karara bağlandı (§8b.8): nötrler W1 kuralında (A27 kanıtla
   açık); tempo her tick (A25 tetiksiz); sayaçlar metrik yolunda (A26
   kapandı); registry bandı tetiksiz (A13); `Welcome { faction,
   factions }` `Private.game`'de.*

### 8b.8 W2 — röle gerçek yük altında (branch `demo/w2-war`)

W2 kompozitin üstüne doğrulama oyununu ("Cephe", `gsb-demo-war`;
KIT-ARCHITECTURE §10 "W2 sonucu"), sunucu modülünü (`game = "war"`) ve
loadgen botunu (`--game war`; GAME-MODULE "W2 sonucu") kurdu. Kit ve
kompozit **değişmedi**; bu bölüm rölenin gerçek yük altındaki
sayılarıdır.

**Sayaçlar artık metrik yolunda (A26 kapandı).** §8b.6'nın gerekçesi
("tetik: W2 ölçüm isterse") gerçekleşti: ölçüm A25/A13 kararı için
oranlara ihtiyaç duydu. Shard'ın `TeamStats`'ı artık kümülatif ve
`RoomSample`'da yedi alan (`team_exports`, `team_export_drops`,
`team_export_records`, `team_over_cap`, `team_imports`,
`team_import_records`, `team_expired`) → rapor, `gsb-metric` satırı,
Prometheus (`gsb_room_team_*_total`, OPS §3), loadgen metrik teli (magic
`GSMA`, onuncu düzen; katlama SUM). `team_exchange_summary` satırı ~1 sn
penceresini tutuyor (son yazdığı anlık görüntüden fark,
`TeamStats::since`). Hub tarafı (`relays`, `relay_drops`) log satırında
kaldı: shard'a varan her import düşmemiş bir röledir, yani `imports /
exports` yayılımı ve kayıpları shard tarafından okunur. RESULT, isteyen
bot için (savaş) kararlı pencerede `team_exports_s`,
`team_export_records_s`, `team_records_per_export`, `team_imports_s`,
`team_import_records_s`, `team_fanout` ve toplamlar (`team_export_drops`,
`team_over_cap`, `team_expired`, `migrations`, `effects_applied`) basar.
*A29:* sekizinci alan `team_over_budget` — mantığın KENDİ bütçesinin
kestiği kayıt (export'la raporlanır, §8b.1), Prometheus
`gsb_room_team_over_budget_total`, pencerede `over_budget`, tel magic'i
`GSMB` (`GSMA` + `team_expired`'dan sonra bu alan), RESULT'ta
`team_over_cap`'ten sonra `team_over_budget`.

**Ölçüm** (release, `gsb-loadgen N --game war --duration 10
--write-stall-secs 0`; 1000: `--orchestrate 1000 --procs 2`; rUDP:
`500 ... --transport udp --stagger-ms 5`; makine başka ajanlarla
paylaşımlı — 1 dk yük ortalaması tabloda, 32 çekirdek). Her koşuda
`left = N`, `errors=0`, `server_closes=0`, `server_hz` 29,95–30,01,
`step_over_budget_pct=0.0`:

| N | Yük | `team_exports_s` | kayıt/export | `team_export_records_s` | `team_imports_s` | `team_import_records_s` | yayılım | drops / over_cap / expired | göç (10 sn) | uzak etki |
|---|---|---|---|---|---|---|---|---|---|---|
| 200 | 28,8 / 36,8 | 120,0 / 119,9 | 139,5 / 140,1 | 16 741 / 16 802 | 359,9 / 359,8 | 50 225 / 50 406 | 3,00 | 0 / 0 / 0 | 11 / 11 | 0 / 0 |
| 500 | 28,4 / 37,2 | 119,9 / 119,8 | 360,8 / 358,7 | 43 252 / 42 971 | 359,6 / 359,4 | 129 852 / 128 986 | 3,00 | 0 / 0 / 0 | 76 / 69 | 5 / 6 |
| 1000 (sep) | 34,4 / 39,1 | 120,0 / 119,9 | 780,0 / 779,5 | 93 629 / 93 462 | 360,1 / 359,7 | 280 968 / 280 456 | 3,00 | 0 / 0 / 0 | 111 / 97 | 7 / 5 |
| 500 rUDP | 44,3 | 119,8 | 358,8 | 43 002 | 359,5 | 129 086 | 3,00 | 0 / 0 / 0 | 62 | 5 |

**Bulgular.**

1. **Tempo sabit, hacim N ile doğrusal.** Her shard'da her tick bir
   export (4 × 30 = 120/sn): her bölgede her fraksiyonun kulesi var,
   yani hiçbir shard'ın takım trafiği sıfıra inmiyor (§8b.3'ün "boşsa
   gönderme" kuralı Cephe'de hiç devreye girmiyor). Export başına kayıt
   ≈ 0,78·N (üç takımın bu shard'daki üyeleri + gördükleri);
   yayılım tam 3,00 — her shard'da her fraksiyonun oyuncusu var, her
   export diğer üç shard'a gidiyor.
2. **Röle kayıpsız ve ucuz.** 1000'de 93,5 k kayıt/sn export, 280,7 k
   kayıt/sn import; registry posta kutusu hiç dolmadı
   (`team_export_drops = 0`), her export'un röleleri vardı (`imports =
   3 × exports`), çekirdek tavanları kesmedi, TTL hiçbir yuvayı
   düşürmedi. Gövdeler `Bytes` refcount'uyla paylaşılıyor (kopya yok):
   1000'de röleden geçen ~16 B'lık kayıt gövdesi saniyede ~4,5 MB'lık
   REFERANS — aynı koşuda istemcilere giden bayt ~365 MB/sn. Registry'nin
   adımı/CPU'su doğrudan gözlenemiyor (tokio görevi, iş parçacığına
   bağlı değil); dolaylı kanıt: sıfır düşme ve sunucunun toplam CPU'su
   (1000: 6,0–6,1 sn, 2 süreçlik orkestre koşusunun tamamı).
3. **Bant röleden değil senaryodan.** İstemci başına görünüm 1000'de
   ~956 birim (333 müttefik + 4 kule + gördüğü ~620 düşman), çoğu her
   tick yeni desimetre değeri: `out_bps_per_conn` 200'de ~65 KB/sn,
   500'de ~175 KB/sn, 1000'de ~365 KB/sn (arena 1000 delta ~210, MMO
   1000 ~102); en büyük kare 3,5 / 9,2 / 18,5 KB (keep-alive full'u),
   rUDP 500'de `frag_reassembled` 129 k / 10 sn, `frag_dropped = 0`.
   "Müttefikler harita genelinde" her istemciye O(N) kayıt/tick'tir;
   ölçeği A10 (varlık başına yayın hızı: uzak müttefik 1–5 Hz'de
   minimap'e yeter) ya da A22 (değer düzeyinde delta) değiştirir, export
   temposu değiştirmez.
4. **İlk bot sürümünde seam ötesi dövüş hiç yoktu.** Karakolların hepsi
   bölgelerin derinindeydi: `effects_applied = 0` (200/500/1000),
   göçler yalnız yeniden doğmalar. Orta noktanın halkası dikişleri
   kesecek kadar genişletildi (70–95 m; `12d7380`): 500/1000'de 10 sn'de
   5–7 uzak etki, 60–110 göç — gerçek yük altında hatasız. Küçük sayı
   botun seyrek saldırısından (≈ 5 sn'de bir) ve dikişe yakın
   karşılaşmaların azlığından.

**Kararlar (BACKLOG).**

- **A25 (export temposu `every k`)** — **gerekmedi, açık kalır.**
  1000'de registry'de sıfır düşme; tempo düşürmek röle mesajlarını k'da
  bire indirir ama istemci baytını değiştirmez (shard her tick kendi
  içeriğini göndermeye devam eder, ithal kayıtlar yalnız bayatlar) ve
  uzak kulenin gördüğü düşmanı k tick geç gösterir. Tetik sıkılaştı:
  `team_export_drops > 0` ya da registry gecikmesi (A13 ile birlikte).
- **A26** — **kapandı** (yukarıda).
- **A27 (harita geneli nötrler)** — Cephe'nin kararı: W1 kuralı kabul
  (sahipsiz ele geçirme noktası kendi shard'ında herkese, başka yerde
  sisle); ele geçirilen nokta fraksiyonun birimi olur (sahibine harita
  geneli). Bulgu (KIT-ARCHITECTURE W2-1): kural oyuncuya görünen bir
  bölüm artefaktı — 640 m ötedeki shard 3 oyuncusu noktayı görür,
  100 m'deki shard 0 oyuncusu görmez. **Açık kalır**, tetik somut: harita
  durumunu herkese göstermek isteyen bir hedef (bayrak, üs sağlığı).
  En küçük kit değişikliği: entity başına "harita geneli" işareti.
- **A28 (göçte bir tick'lik boşluk)** — **ölçülmedi, kabul kalır.**
  Loadgen görünümdeki kısa kayıp-geri gelişleri saymıyor; 1000'de 10
  sn'de ~100 göç. Kit'in gerçek aktör testi boşluğu tam bir tick'le
  sınırlıyor.
- **A13 (registry striping)** — tetik yok (sıfır düşme, 1000'de 120
  export/sn).

## 9. Uygulama durumları

| Kalem | Durum |
|---|---|
| Delta border exchange (§6.4) | 💤 main'de ama **uykuda** — Faz C'den beri süreç-içi link'ler `AlwaysFull`; delta yalnız testlerde (`force_exchange_modes`) koşar, ilk tüketici `Ipc`/`Net` link'i (§7 "Güncel durum") |
| sharded × spatial kompoziti | ✅ Faz B — main'de |
| Ortak delta motoru çıkarımı | ◐ CellBook/CellPieces ~~common.rs'te~~ `gsb-kit/src/common/cells/`'te; T'de `SetLedger`/`Baselines`/`emit_private_full` (üç delta odası ortak); `all`/`pvs` adopsiyonu tetikleyicili (BACKLOG A1) |
| team × sharded (§8, §8b) | ✅ W1 — kompozit + registry hub (§8b.7); ✅ W2 — doğrulama oyunu "Cephe", sunucu modülü, loadgen botu, sayaçlar metrik yolunda (§8b.8) |
| Çoklu-listener (karışık transport istemci) | ✅ ROADMAP — uygulandı |
| Seam ötesi okuma + `RemoteEffect` (§2, §4 katman 1–3) | ✅ C1 — §4b |
| Crystallization (§4 katman 4) | ✅ C2 — §4c (opt-in; MMO açık) |
| Göç tick'i: ölümlü kopyaya yerel darbe | ✅ D — §4d |

## 10. NOT-DONE

- ~~Cross-seam combat/interaction mesaj tiplerinin implementasyonu~~ —
  C1'de uygulandı (§4b); crystallization C2'de (§4c); kalan:
  saldırı-anında güncel borrow (katman 1'in seçeneği)
- Ortak fizik uzlaşması (§5)
- Adaptif border genişliği (ölçüm öncesi optimizasyon)
- Dağıtımlı (multi-process) shard topolojisi — ufuk katmanı
