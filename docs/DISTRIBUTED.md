# gsb: Dağıtık Shard Topolojisi Tasarımı — ShardLink Sözleşmesi

> Durum: TASARIM (ShardLink arayüzü bugün yalnız in-process
> implementasyonuyla sabitlenir; Ipc/Net implementasyonları tetikleyici
> gerçekleşince). Tetikleyiciler §10'da. Bu doküman dış danışma
> diyaloğunun tamamını sözleşmeye çevirir.

## 1. Hedef senaryo ve kapsam kararı

Hedef: **aynı kabinde kablo ile bağlı 2 makine** ya da **NUMA'lı tek
makinede soket başına bir process (UDS IPC)** — kıtalar arası tek dünya
DEĞİL (fizik izin vermez; kıta-başına ayrı dünya + matchmaking doğru
desendir). Oyun-tasarım sinerjisi: sınıra yakın NPC/şehir konulmaz,
görevler içeride tutulur → cross-shard etkileşim tasarım gereği minimal.

| Ortam | Tek-yön gecikme | Tick bütçesine oranı | Güvenilirlik |
|---|---|---|---|
| In-proc mpsc | ~0.1–1 µs | %0.003 | FIFO, try_send drop hariç |
| UDS (IPC) | ~1–5 µs | %0.015 | Güvenilir+sıralı |
| Kablo/switch | ~100–200 µs | %0.6 | TCP/quinn ile güvenilir |

Üçü de 33 ms bütçenin <%1'i — gecikme sorun değil; **mühendislik
maliyeti başka yerde** (§5–§7).

## 2. Yerleşim hiyerarşisi (dış karar)

```
Dünya (örn. 100×100 shard)
└─ Makine (örn. çeyrekler: 50×50 shard)
   └─ Process (örn. 25×25 shard, NUMA-soket affinity)
      └─ Shard aktörleri (5×5 bloklar halinde gruplanır)
Pahalı bağlantılar YALNIZCA makine/process sınır çiftlerinde;
içerdeki tüm komşuluklar ucuz kanallar. Oyun-dizaynı (sınıra içerik
koymama) cross-makine trafiğini kökten minimize eder.
```

Yerleşim haritası (hangi shard hangi process'te) **bildirimseldir**
(config veya orchestration); otomatik balancer sonraki fazdır.
`border_mode` global config DEĞİLDİR: **bağlantı sınıfından türetilir**
— aynı process komşusu → always-full (byte bedava, CPU kıt);
cross-process/cross-machine komşusu → delta (byte transport parası).

## 3. `ShardLink` sözleşmesi

Shard↔komşu iletişiminin tümü bu arayüzün arkasına alınır (bugün:
neighbor mailbox'larına dağılan try_send + drain):

```rust
pub(crate) trait ShardLink: Send {
    /// En-iyi-çaba gönderim: doluysa düşer (bugünkü try_send semantiği).
    /// Düşen mesajın sınıfına göre telafi kuralı §4'tedir.
    fn send(&mut self, msg: NeighborMsg) -> Result<(), LinkFull>;
    /// CONTROL fazındaki drain: gönderim sırasıyla teslim (FIFO).
    fn drain(&mut self) -> Vec<NeighborMsg>;
}
```

Implementasyonlar:

| | `InProcLink` (bugün) | `IpcLink` (UDS) | `NetLink` (kablo/TCP-quinn) |
|---|---|---|---|
| Taşıma | bounded mpsc | Unix socket stream | TCP ya da quinn |
| Kayıp | try_send drop | yok (stream) | yok (stream) |
| Serileştirme | yok (move) | var (yeni kod yüzeyi) | var |
| Kanal-move migrasyonu | ✅ çalışır | ❌ → session-relay (§6) | ❌ → session-relay |

Aktördeki değişiklik yalnız çağrı noktalarının link'e devridir; mesaj
semantiği hiçbir yerde değişmez (üç mesaj sınıfı zaten kayıp-toleranslı
tasarlandı — §4).

## 4. Mesaj sınıfları: kayıp/gecikme/sıralama matrisi

### Sınıf ANTI-ENTROPY — Border exchange
- Bugün: düşen exchange sonraki tick'in full'u/delta'sı tarafından
  onarılır; backpressure-drop'ta gönderici forced-Full bayrağı (bir tick
  içinde iyileşme — ölçüldü).
- Dağıtımda: aynı desen; delta byte'ları gerçek ağdan akar (delta'nın
  kazancı tam da burada — ölçüm: 0.39×).

### Sınıf EXACTLY-ONCE-EVENT — Migrate / Resume
- Bugün: epoch guard + try_send başarısızlığında rollback (kanal uçları
  geri döner). Dağıtımda ek gereken: **ACK + journal** — sender,
  ACK gördüğü tick'e kadar Migrate'i journal'da tutar; `T_migrate_timeout`
  (tick-tabanlı) içinde ACK gelmezse entity sender'da yeniden kurulur.
  Geç gelen eski Migrate epoch guard tarafından reddedilir → iki tarafta
  da tek sahiplik korunur (split-brain yok).

### Sınıf EFFECT — RemoteEffect (sniper/büyü/melee)
- İdempotent komutlar; sıralama çözümü GLOBAL DEĞİL:
  - **Per-entity seq:** efekt `(hedef_kimliği, kaynak_shard, seq)` taşır;
    alıcı yalnız gördüğünden yeniyi uygular, eskiyi atar → entity-başına
    toplam sıra (global sıra aranmaz — bedeli ödenmez).
  - **Staleness politikası:** efekt kaynak-tick damgalı; `K` tick'i aşan
    etki oyun-politikası olarak reddedilir (30 tick gecikmiş melee hit
    uygulanmamalıdır). K logic'e aittir; base damgayı taşır.
  - Rota: hedef kimliği başka shard'a geçtiyse **forwarding** — eski
    sahip yeni sahibe yönlendirir (zincir derinliği migrasyon başına
    sınırlı) ya da registry ownership dizisine sorar (Faz kararı).
  - **Uygulandı (C1, CROSS-SHARD §4b):** `ShardMsg::RemoteEffect` —
    `ShardLink`'in taşıdığı YENİ mesaj. Rota forwarding (eski sahip,
    Migrate'i kuyruğa soktuğu anda `wire → yeni sahip` kaydı tutar, TTL
    11 tick, ≤ 3 atlama); registry'ye sorma elendi (sıcak yolda registry).
    Sapma: "yalnız gördüğünden yeniyi uygula" yerine köken-shard başına
    KAYAN PENCERE — yönlendirilen/yeniden denenen gerçek efekt daha
    yenisinden sonra gelebilir. Tam kanalda sınırlı yeniden deneme (bir
    sonraki tick, yaş tavanı 7 tick).
  - **Gelecek codec:** zarf yalnız tamsayılardır (`target`, `source`,
    `EffectId { origin, epoch, seq }`, `at_tick`, `hops`), yük opak
    `Bytes`'tır — bu mesajın codec'i Migrate.State / BorderRecord'un
    aksine ÇEKİRDEĞE aittir (oyun tipi yok; §4b ilkesi yükün codec'ini
    zaten oyuna vermiş durumda). Ipc/Net link'i bunu kapsamalı; ağda
    at-least-once teslimde dedup penceresi zaten hazır.

## 4b. Serileştirme sahipliği: tipler logic'te, codec de logic'te

Ipc/Net linklerde kapı 2 (Migrate.State) ve kapı 3 (BorderRecord)
tipleri byte'a dönüşmek zorundadır (process bellekleri ayrıdır, move
yok). İlke: **tipi kim tanımlıyorsa codec'i de o tanımlar** — yani
logic; core hiçbir oyun alanını serileştirmez.

Mekanizma: `ShardLink` implementasyonları payload tipleri üzerinde
generic'tir (`Encode/Decode` bound'ları logic'in tiplerinden gelir);
`InProcLink` bu bound'ları görmezden gelir (move yeterli), `Ipc/NetLink`
uygular. Aktör kodu hiçbir şekilde değişmez; yalnız takılı link
değişir. Doğrulama: her logic tipi için codec round-trip testi
(DISTRIBUTED §9 fault-injection planının yanına ekli).

Ayrım — kapı 2 tam State taşır (oyuncunun tümü), kapı 3 yalnız
görünürlük dilimi alt-setini taşır: tipler bilinçli olarak FARKLIDIR,
birleşen şey sahiplik desenidir ("tipi logic tanımlar"). Kapı 1
(client snapshot) ise ara-tip olmadan doğrudan wire'a yazım ile
aynı ilkeyi paylaşır (performans gereği fuse edilmiş — DESIGN/TRAIT
belgeleri).

Kapı 2'nin kanal-move kırılımı ayrı konudur: state serileştirilerek
geçer AMA oyuncu frame'leri eski process'ten relay edilir (§6
session-relay).

## 5. Sıralama ilkesi (özet)

Global mesaj sırası HİÇBİR şarta garanti edilmez (bedeli ödenmez).
Garanti edilen: (1) aynı link'te FIFO, (2) entity-başına versiyon sırası,
(3) deterministik hakem kuralları (`(attacker_id, seq)` kıyası —
CROSS-SHARD §4.3), (4) staleness politikası. Bu dörtlü oyun-doğru
davranış için yeterlidir; kanıt her kilit testiyle yapılır.

## 6. Session-relay: kanal-move'un process sınırında kırılması

Bugün oyuncu shard değiştirince kanal uçları move edilir (bağlantı
actor'ü fark etmez). Process sınırı bunu imkânsız kılar:

- **Bağlantı actor'ü bağlı olduğu process'te KALIR**; entity devri
  sonrası frame'ler eski process üzerinden **UDS relay hattıyla** yeni
  process'e aktarılır (µs seviyesi — oyuncu fark etmez).
- Relay hattı, bağlantı doğal olarak yeni process'in makinesine/prosesine
  reconnect edene kadar yaşar (opsiyonel optimizasyon; v1'de kalıcı relay
  yeterli).
- Migrate mesajı artık kanal uçlarını değil, relay-rota bilgisi taşır.
- **RPC de relay'den geçer:** bağlantısı A process'inde, entity'si B'de
  olan oyuncunun `RPC_REQ` zarfı relay hattından B'deki shard actor'üne
  ulaşır; pending set/timeout sweep/yanıt (private-frame) B'de işler,
  yanıt aynı relay'den döner. Bedel: request başına bir ek hop (UDS'te
  µs, kabloya sub-ms — kabul). Sonuç: RPC yoğun oyuncular için
  crystallization/co-location gerekçesi güçlenir (sürekli hop'lu
  trafiği önlemek için).

## 6b. Bağlantı devri (rehome): relay geçici köprüdür, kalıcı çözüm değil

İlke (dış karar): relay sonsuz yaşamaz — entity başka makineye geçtiyse
**bağlantı da o makineye gitmelidir**; aksi halde eski makine, artık
kendi simülasyonunda olmayan oyuncuların trafiğini taşıyan gereksiz yük
merkezine dönüşür.

### Rehome protokolü (tasarım)

```
1. Karar: registry/orchestrator "oyuncu X'in evi artık Makine B"
   (migrasyon sonrası yoğunluk, decommission, dengeleme)
2. A: entity PARK edilir (mevcut resume mekanizması)
3. A: B'ye relay köprüsü ANINDA açılır  ← bildirimden ÖNCE
   (köprü hazır olmadan istemciye haber verilirse paket kaybı olur;
    sıralama bilinçli: önce köprü, sonra duyuru)
4. A → client: REHOME frame { yeni adres, resumption-token }
5. Client: B'ye yeni bağlantı (QUIC ise 0-RTT resumption; TCP+TLS ise
   session ticket), AUTH(ticket) + RESUME → park defterinden geri bağlanır,
   AYNI wire id ile devam eder
6. Köprü boşalır (istekteki frame'ler aktarıldı) → A tarafı kapanır
```

Kritik özellik: **park/resume mekanizması bu protokolün primitifidir**
— reconnect turunda kurulan altyapı (ticket kimliği → park defteri →
RESUME → aynı wire id) makine-devrinin ta kendisini sağlar. Yeni
gerekli parça yalnızca `REHOME` mesajı ve orchestration kararıdır.

### İstemci taşıma matrisi: QUIC her yerde çalışmaz → çoklu protokol ŞART

QUIC = UDP/443 üstünde. Gerçekçi destek tablosu:

| Ortam | QUIC | Not |
|---|---|---|
| Modern OS/bulut/çoğu ev ağı | ✅ | Tercih edilen |
| Kurumsal ağlar (bir kısmı) | ❌ UDP bloklu/kısıtlı | ~%3-10'lık gerçek dilim |
| Mobil operatörler | ✅ çoğunlukla | Nadiren throttle |
| Unity (MsQuic) | Koşullu | Win11+/Server2022 gömülü; Linux'ta libmsquic kurulumu |
| Konsol/WebGL | ❌ / WebGL'de UDP zaten yasak | Fallback zorunlu |

**Sonuç:** sistem birden fazla protokol DESTEKLEMELİDİR — ve mimari
zaten buna göre kurgulu: `Transport` trait'i (tcp / tls / udp-rudp /
quinn-QUIC hepsi aynı üç trait'i implement eder). Önerilen fallback
zinciri:

```
QUIC (tercih edilen: connection-migration + 0-RTT avantajları)
  └─ düşerse → TLS-TCP (evrensel; rehome protokolü transport-
                bağımsız olduğu için devri burada da çalışır,
                yalnız 0-RTT hızını kaybedersin)
     └─ WebSocket — tarayıcı/WebGL derlemeleri için (kapı VAR:
        `gsb-net` `ws/`, çoklu-listener'da `[[listeners]]` kapısı)
```

Rehome protokolünün kendisi transport-bağımsız tasarlanmalıdır:
`REHOME` frame'i oyun-bandında taşınır, resumption detayı taşımanın
kendisine bağlıdır (QUIC token ya da TLS ticket fark etmez).

## 7. Paylaşılan servisler ve tick senkronu

- **Registry/ticker/result-sink/economy:** tek process SAHİPTİR; diğer
  process'ler IPC istemcisidir. Join/auth yolları bir hop alır (düşük
  frekans — kabul). Sahip process ölürse: süreç-yönetimi restart eder,
  state bellektir (kalıcılık katmanı hâlâ kapsam dışı).
- **Tick senkronu:** her process kendi ticker'ına sahiptir (wall-clock
  resync); sayaçlar zamanla kayar. Çözüm adayları: (a) periyodik
  tick-offset duyurusu (link üstünden), (b) cross-process mesajlarda
  `at_tick` yerine deadline semantiği. Karar implementasyon turuna ait;
  ikisi de elenebilir alternatiflerle burada belgelenecek.

## 8. NUMA bölümü

Soket-başına process + CPU affinity pinning: memory locality kazanımı +
hata izolasyonu + kullanılabilir çekirdeğin ikiye bölünmemesi.
**Önce ölç:** numactl pinli/pinsiz mevcut sharded yük karşılaştırması
(yarım gün) — kazanç iş yüküne göre değişir, varsayımla yapılmaz.

## 9. Fault-injection test planı

NetLink'in kayıp/gecikme/enjeksiyon davranışı test edilebilir olmalı
(loopback'te-çalışıyor tuzağını kırmak için): link implementasyonuna
enjeksiyon kancası (drop% / delay ms / reorder penceresi parametreli,
yalnız test modunda). Her sınıf için: kayıp → telafi; gecikme →
staleness; partition → degrade-mode (§10) testleri.

## 10. Tetikleyiciler ve faz planı

| Tetikleyici | Aksiyon |
|---|---|
| Tek soketin çekirdekleri dolmaya başlar | numactl ölçümü → IpcLink uygulaması (UDS, 2 process) |
| NUMA ölçümü pozitif | Process-hiyerarşisi rollout (placement config) |
| İki makineye çıkma kararı | NetLink (quinn/TCP) + session-relay + tick-senkron |
| Cross-machine gerçek oyun ihtiyacı | Crystallization agresifleştirme (RTT'de zorunlu) |

## 11. NOT-DONE

- Kıtalar arası tek dünya (fiziksel imkânsız — bölgesel instancing)
- Global mesaj sıralaması (Lamport/vector clock) — bedeli ödenmez
- Dağıtılmış kilit/joint-authority — CROSS-SHARD §5
- Otomatik shard-balancer (bildirimsel placement yeter v1'de)
- Kalıcılık katmanı — tasarım `docs/PERSISTENCE.md`'e taşındı (iki sınıf:
  maç-checkpoint clone deseni + MMO persistence-service; uygulama tetikleyicili)
