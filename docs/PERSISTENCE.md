# gsb: Kalıcılık Tasarımı — İki Ayrı Sınıf, İki Ayrı Çözüm

> Durum: TASARIM NOTU (dış danışma diyaloğundan derlendi). Uygulama
> tetikleyicilere bağlıdır (§5). Temel ilke: kalıcılık TEK bir şey
> değildir — maç oyunları ile kalıcı dünya oyunlarının ihtiyaçları
> farklıdır ve farklı çözülür.

## 1. İki sınıf

| | **Maç oyunları** (MOBA/FPS) | **Kalıcı dünya** (MMO) |
|---|---|---|
| Verinin anlamı | Maç boyunca yaşar, maçla ölür | Oyuncu/dünya ile yaşar, maçı aşar |
| Kaybın bedeli | Maç terk edilebilir | Kalıcı ilerleme kaybı — kabul edilemez |
| Otoriter depo | Süreç belleği (+ opsiyonel checkpoint) | Merkezi kalıcı katman (DB/hizmet) |
| Game server rolü | Sahip + simülatör | Cache + oturum-içi değişiklik |

## 2. Maç oyunları: her-tick tip-li checkpoint (clone semantiği)

### Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | Kadans: HER TICK | Önceki tick'te işlenmiş kritik kararın (ultı, item, öldürücü vuruş) kaybedilmemesi için ara-granülarite yetmez |
| 2 | **Clone, encode ASLA** | Checkpoint process'i terk etmez — codec'in varlık sebebi (sınır geçişi) yoktur. Typed clone = memcpy seviyesi hız + tip güvenliği + yırtılmasızlık |
| 3 | Tip sahipliği: `type MatchCheckpoint` logic'te (`BorderRecord<Strip>` deseni) | Hangi alanların kopyalanacağına oyun karar verir (pozisyon+hp+cooldown+buff+skor+süre...) |
| 4 | **Çift-tampon (ping-pong)** | Kopya senkron tick-işi olduğundan yırtılma yapısal olarak imkânsız; çift-tampon ek olarak "checkpoint yazımı sırasında gelen ikinci çökme"ye karşı sigortadır |
| 5 | Çoklu-çökme devre kesici: aynı maçta ≥3 çökme → maç terk | İhlal-bütçesi felsefesiyle aynı desen; tekrarlanan çökmede oyuncular zaten devam istemez |
| 6 | Restore: boş odadan değil, checkpoint'ten kur; bağlantılar zaten öldüğü için oyuncular RESUME akışıyla (ticket → park defteri → aynı wire id) döner | Mevcut reconnect altyapısı doğrudan yeniden kullanılır |

### Maliyet (ölçek: ~200 varlık)

200 varlık × ~100–200 B ilgili alan ≈ 20–40 KB kopya/tick × 30 Hz ≈
~1 MB/s saf memcpy — mikrosaniyeler seviyesinde; bütçe gürültüsü.
(Karşılaştırma: 10k varlık varsayımı YANLIŞ senaryoydu — maç
oyunlarında varlık nüfusu tasarım gereği sayılıdır.)

### Core/logic bölüşümü (üç kapı ilkesiyle uyumlu)

```rust
trait GameLogic<W>: ... {
    /// Maç checkpoint'i: hangi alanların kopyalanacağına oyun karar verir
    type MatchCheckpoint: Clone + Send;
    fn checkpoint(&self, world: &W) -> Self::MatchCheckpoint;
    fn restore(&mut self, world: &mut W, cp: Self::MatchCheckpoint);
}
```
Core: kadans yönetimi + çift-tampon saklama + çökme sonrası
`restore` çağrısı. İçerik bilmez.

## 3. Kalıcı dünya (MMO): üç veri sınıfı, üç strateji

| Veri | Strateji |
|---|---|
| Ayrık olaylar (ticaret, item, kill, quest) | Anında event-yazımı — RPC-External delegasyonuyla persistence-service'e |
| Akıcı durum (pozisyon, hp-regen) | Periyodik checkpoint (30–60 sn) |
| Zorunlu flush | Logout / bölge geçişi / tick-down başlangıcı |

Çökme anında oyuncular son checkpoint'e geri sarılır ("save-point
rubber-band") — endüstride kabul görmüş davranış; en fazla checkpoint
aralığı kadar ilerleme kaybı.

### Kim atar? — her shard kendi, merkez koordinatör DEĞİL

| Kriter | Shard-başına push | Merkez supervision koordinatörü |
|---|---|---|
| Kirliyi bilme | ✅ Change-detection shard'da | ❌ Sormak zorunda = ek protokol |
| Aktör disiplini | ✅ RPC-External mevcut desen | ❌ Yeni awaited kaynak |
| Darboğaz | Yok (shard başına bağımsız) | Merkez bottleneck |

Desen: shard kirli oyuncu verilerini ENCODE ederek (burada sınır
geçilir — DISTRIBUTED §4b codec sahipliği işler) bounded kanala iter;
PersistenceWriter görevi batch'leyip kalıcı kata yazar. Logout =
anında flush + ACK.

### Otorite modeli

Kalıcı otorite MERKEZİ KATMANDADIR (DB/hizmetler); game server cache +
oturum-içi değişiklik sahibidir. Game server'da otoriter kalıcı veri
tutmak: makine kaybı=veri kaybı, redeploy=kesinti, yatay ölçek
imkânsız — üçü de kabul edilemez.

## 4. Elenen alternatifler

- **Core'a `WorldPersistence` trait'i** — kalıcılık core'un concern'u
  değildir; delegasyon altyapısı (RPC-External) zaten vardır. ELENDİ.
- **Client-snapshot encode'unun checkpoint olarak yeniden kullanımı** —
  client paketi görüntü-filtreli ve proto-encoded'dır; checkpoint
  simülasyon-devamı verisidir. Farklı veri, farklı yol. ELENDİ.
- **Her-tick byte-serileştirme** — clone semantiği yerine encode: sınır
  geçmediği için gereksiz CPU + tip güvenliği kaybı. ELENDİ.
- **Merkezi persistence koordinatörü** — kirliyi bilmez, yeni awaited
  kaynak yaratır. ELENDİ (shard-başına push kazandı).

## 5. Test planı (tetikleyici gerçekleşince)

1. checkpoint_roundtrip_restores_world_state
2. crash_between_ticks_leaves_previous_checkpoint_intact (çift-tampon)
3. three_crashes_abandon_the_match (devre kesici)
4. resume_binds_parked_players_to_restored_entities (aynı wire id)
5. mmo: logout_flushes_before_session_release; periodic_checkpoint_
   bounds_rollback_window; event_writes_are_immediate

## 6. Tetikleyiciler ve faz planı

| Tetikleyici | Faz |
|---|---|
| Gerçek maç oyunu tek-makine çökme sıklığı rahatsız edici olur | Maç checkpoint'i (§2) |
| MMO haritası gerçek oyuncularla açılır | Persistence-service deseni (§3) |
| Şimdilik | İkisi de bekler — mevcut davranış (boş rebuild + temiz hata) zaten tutarlı |
