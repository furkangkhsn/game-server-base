# scripts/

Bakımcının elle koştuğu ölçüm betikleri. Test takımının parçası değiller
(CI koşmaz); her biri kendi başlığında ne yaptığını, neden o sayıları
seçtiğini ve neye dokunduğunu yazar.

## `rudp-jitter.sh` — B104: `udp_congestion` varsayılanı `"pace"` olabilir mi?

Aynı rUDP yükünü `--udp-congestion off` ve `pace` ile, loopback'e
(`lo`) netem ile titreşim / kayıp / gerçek darboğaz koyarak koşar ve tek
bir tablo basar. Soru iki yarıdır:

1. **Darboğazsız titreşim hızlanmayı tetiklememeli** — bölüm
   (`episodes`), hız kesme (`cuts`), hızlanmanın düşürdüğü ya da
   beklettiği kare (`dropped`, `queued`) `pace` satırlarında da ~0
   olmalı; yoksa varsayılan `pace` sağlıklı ama titrek yoldaki oyuncuyu
   kısar.
2. **Gerçek darboğaz tetiklemeli** — `bottleneck` ve `bottleneck_jitter`
   satırlarında `pace`'te bölüm ve kesme görülmeli; `dropped` yolun değil
   hızlanmanın düşürdüğüdür (`lost%` `off`'a göre düşmeli).

### Koşmak

```sh
# Tam ölçüm (varsayılan: 64 istemci, 30 sn, her senaryo×kip 3 koşu —
# yedi senaryoda ~14 dk). sudo parola sorabilir.
scripts/rudp-jitter.sh

# Daha kısa / seçili:
CLIENTS=128 DURATION=20 RUNS=2 SCENARIOS="baseline jitter60 bottleneck" \
  scripts/rudp-jitter.sh

# Tesisat denemesi: hiçbir sudo/tc çağrısı yok, her senaryo şekilsiz koşar
# (yalnız betiğin, RESULT ayrıştırmasının ve tablonun çalıştığını gösterir).
RUDP_JITTER_DRY=1 CLIENTS=8 DURATION=4 RUNS=1 scripts/rudp-jitter.sh
```

Ortam değişkenleri betiğin başlığında: `CLIENTS`, `DURATION`, `RUNS`,
`SCENARIOS`, `LOADGEN_MODE` (`orchestrate` varsayılan: sunucu ve
istemciler ayrı süreçlerde; ya da `inproc`), `LOADGEN_EXTRA` (ör.
`--game arena`), `BOTTLENECK_RATE` (sabit `tc` hızı, ör. `4mbit`;
verilmezse baseline'ın ölçtüğü loopback talebinin `BOTTLENECK_FRAC`'ı,
vars. 0,5), `BOTTLENECK_LIMIT` (netem kuyruğu, vars. 1000 paket),
`OUT_DIR`.

### Güvenlik

- netem `lo`'daki **bütün** trafiği şekillendirir; betik koşarken
  makinedeki her loopback servisi (veritabanı, IDE, yerel sunucular)
  gecikir. Makine başka işle meşgul olmamalı — aç kalan süreç de titreşim
  gibi görünür.
- `lo`'da varsayılan `noqueue` dışında bir kök qdisc varsa betik hiç
  başlamaz (başkasının kurduğuna dokunmaz). Kendi eklediğini her çıkışta
  siler (normal bitiş, hata, Ctrl-C, TERM). Silemezse elle:
  `sudo tc qdisc del dev lo root`.
- Gecikmeler YÖN başınadır (loopback datagramı `lo`'dan yön başına bir kez
  çıkar): `jitter20` ≈ 40 ms RTT. netem titreşimi datagramları yeniden
  sıralar — gerçek yolların çoğundan serttir.

### Çıktı ve bakılacaklar

`target/rudp-jitter/<zaman>/` altında: her koşunun tam çıktısı
(`<senaryo>/<kip>-<koşu>.log`), RESULT satırları (`results.tsv`),
koşu bilgisi (`run.txt`: parametreler, çekirdek, türetilen darboğaz hızı)
ve tablo (`summary.txt`, ekrana da basılır). Sütunlar:

| Sütun | Kaynak (RESULT) | Ne söyler |
|---|---|---|
| `episodes`, `cuts` | `transport_udp_game_paced_episodes`, `…_rate_cuts` | hızlanma başladı mı, kaç kez kısıldı (koşu ortalaması) |
| `dropped`, `queued` | `…_frames_dropped_paced`, `…_frames_queued_paced` | hızlanmanın düşürdüğü / beklettiği oyun bandı kareleri |
| `lost%` | `…_datagrams_reported_lost` ÷ `…_reported_sent` | istemcilerin bildirdiği yol kaybı |
| `rtt_ms` | `…_rtt_sum_us` ÷ `…_rtt_samples` | ortalama sonda turu |
| `conn_p99`, `p99_max` | `connect_p99_ms` | el sıkışma gecikmesi (ortalama, en kötü koşu) |
| `snaps/s` | `snap_per_s` | istemcilerin aldığı toplam snapshot hızı |
| `ends` | `udp_ends_*` | istemcinin bitirdiği oturumlar (B128) — 0 olmalı |

**Karar için:** `jitter*` satırlarında `pace`'in `episodes`/`cuts`/
`dropped` değerleri `off`'unkine (hep 0) yakın ve `snaps/s` aynıysa
titreşim yanlış hızlanma üretmiyor; `bottleneck*` satırlarında `pace`
bölüm açıp kesiyor ve `lost%` `off`'tan düşükse darboğaz görülüyor. İkisi
de tutarsa `udp_congestion` varsayılanı `"pace"` olabilir. Sonucu ve
`summary.txt`'yi BACKLOG B104'e işleyin.
