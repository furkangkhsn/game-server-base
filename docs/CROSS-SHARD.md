# gsb: Cross-Shard Etkileşim ve Border Paylaşım Tasarımı

> Durum: TASARIM NOTU (dış danışma diyaloğundan derlendi); §2–§4'ün
> uzak-etki kısmı UYGULANDI (§4b). §2–§5 etkileşim
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
8. *Katman 4 (crystallization)* sonraki tur.

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
| Crystallization (§4 katman 4) | 🔜 sonraki tur |

## 8. NOT-DONE

- ~~Cross-seam combat/interaction mesaj tiplerinin implementasyonu~~ —
  C1'de uygulandı (§4b); kalan: crystallization (§4 katman 4) ve
  saldırı-anında güncel borrow (katman 1'in seçeneği)
- Ortak fizik uzlaşması (§5)
- Adaptif border genişliği (ölçüm öncesi optimizasyon)
- Dağıtımlı (multi-process) shard topolojisi — ufuk katmanı
