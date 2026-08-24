# gsb: Cross-Shard Etkileşim ve Border Paylaşım Tasarımı

> Durum: TASARIM NOTU (dış danışma diyaloğundan derlendi). §2–§5 etkileşim
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

## 7. Ölçüm planı (önce veri)

1. **Faz 0 (main):** mevcut BORDER fazını enstrümante et — byte/tick,
   kayıt/tick, encode µs, send-drop sayacı; sharded senaryolarda ölç
   (4 shard orta yoğunluk + orchestrator yüksek yoğunluk). Ham sayılar
   ROADMAP'e işlenir.
2. **Faz 1 (branch `border-delta`):** §6.4 tasarımının implementasyonu,
   aynı enstrümantasyonla aynı senaryolar.
3. **Karar kriteri:** kazanç ölçülebilir VE karmaşıklık bedeline değiyorsa
   branch main'e alınır; değilse ölçüm kaydıyla branch arşivlenir.
   (Proje ilkesi: önce veri.)

## 8. NOT-DONE

- Cross-seam combat/interaction mesaj tiplerinin implementasyonu (§2–§4
  tasarım; uygulama ayrı tur)
- Ortak fizik uzlaşması (§5)
- Adaptif border genişliği (ölçüm öncesi optimizasyon)
- Dağıtımlı (multi-process) shard topolojisi — ufuk katmanı
