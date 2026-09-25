# gsb: Ops Yüzeyi Tasarımı — /metrics, /healthz, Admin API

> Durum: TASARIM (uygulama bu dokümanı sözleşme alır). Reconnect sonrası
> sunucu uzun ömürlü durum tutuyor (park edilmiş oyuncu, pending RPC,
> oturum sayaçları) — gözlemlenemeyen durum kör noktadır; bu doküman
> mevcut metrik/kontrol hattını HTTP'ye açar.

## 1. Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | HTTP sunucusu **elle yazılmış minimal HTTP/1.1** (tokio `TcpListener` üstünde) | Sıfır yeni dependency; Prometheus text formatı ve 3 endpoint için framework ağır gelir. Projenin kendi geleneği (rUDP, framing, lint hep elle). Elenenler: axum/hyper — bağımlılık bütçesi + yüzey alanı. Yalnız `GET` + `Connection: close`; keep-alive/chunking yok (NOT-DONE) |
| 2 | Metrik akışı **`tokio::sync::watch`** ile | `MetricSink` enum'üne üçüncü varyant: `Watch(watch::Sender<MetricReport>)`. Collector her rapor periyodunda en son raporu watch'a iter; HTTP task `borrow()` ile okur. Kilit yok (tek yazar, tek güncel değer — tam watch semantiği); lint'e takılan paylaşımlı-state deseni de girmez |
| 3 | **JSON yok** — admin parametreleri query-string/form-encoded | Elle JSON parser'ı yazmak red; admin yüzeyi 3 operasyon, JSON gerektirmiyor. Elenen: serde_json bağımlılığı |
| 4 | Varsayılan bağlama **127.0.0.1**, config ile açılır/kapatılır | `http_listen = ""` → devre dışı (varsayılan davranış değişmesin); adres verilirse dinler. v1'de auth yoktur (NOT-DONE): localhost dışına açmak operatörün bilinçli kararıdır ve dokümanda uyarılır |
| 5 | Prometheus **text exposition formatı** (`# HELP/# TYPE` + örnek satırları) | Standart scrape formatı; `MetricReport` alanlarından üreten ayrı render fonksiyonu (unit-test edilebilir, HTTP'den bağımsız) |

## 2. Endpoint'ler

| Yöntem | Yol | İşlev |
|---|---|---|
| GET | `/healthz` | `200 ok` — süreç ayakta + ticker yaşıyor mu (son rapor yaşı eşiği aşmadıysa ok; aşıysa 503, reason body'de) |
| GET | `/metrics` | Prometheus text — tüm RegistrySample/RoomSample/ConnSample sayaçları (`gsb_*` önekli) |
| GET | `/rooms` | Tablodaki odalar + durumları (id, members, persistent) — kontrol düzleminin table-only cevabı |
| POST | `/rooms/open?id=&tick_hz=` | Runtime oda açma (`ServerHandle.open_room` idempotent-create sözleşmesiyle) |
| POST | `/rooms/close?id=` | Oda kapatma (`close_room`; persistent ise emeklilik semantiği işler) |

Admin yolları mevcut `ServerHandle` komutlarını kullanır — yeni bir kontrol yolu AÇILMAZ, yalnız transport eklenir.

## 3. Tel/format detayları

- Metrik adlandırma: `gsb_registry_rooms`, `gsb_room_r1_steps_total`,
  `gsb_conn_frames_in_total` gibi `<alan>_<nesne>_<sayaç>_total`;
  histogramlar Prometheus summary/satır çiftiyle (p50/p99 hazır alanlardan)
- `/rooms` çıktısı da insan-okunur düz metin (JSON yok kararıyla tutarlı);
  makine-okunurluk için ileride gerekirse ayrı karar
- HTTP task'inin tek await'i accept `recv`; bağlantı başına kısa ömürlü
  task (istek başına tam okuma + tek yanıt + kapanış) — aktör disiplini
  bozulmaz, select gerekmez

## 4. Test planı

1. `healthz_reports_ok_while_ticker_runs` / liveness 503 dalı
2. `metrics_endpoint_exposes_known_counters` — bilinen bir sayacı
   artıran senaryo + scrape'ta görünürlük
3. `admin_open_status_close_round_trip` — runtime oda yaşam döngüsü
4. `disabled_by_default_and_binds_when_configured` — varsayılan kapalı,
   config'li çalışma
5. Prometheus render fonksiyonunun unit testleri (HTTP'den bağımsız)

## 5. NOT-DONE (v1)

- Auth/TLS (localhost sözleşmesi ile yaşar), keep-alive/chunked,
  JSON/protobuf çıktı, /debug/pprof tarzı profillendirme, ops HTTP için
  çoklu-listener (admin/metrics sunucusu tek `http_listen` adresinde
  dinler). *Not: oyun taşımalarının çoklu-listener'ı (`[[listeners]]`:
  tcp/tls/udp/quic/ws) ayrı bir iştir ve yapıldı — CHANGELOG
  "çoklu-listener'a QUIC + WS kapıları turu", ROADMAP devam notu; bu
  madde yalnız ops HTTP'yi kasteder.*

## 6. Kenara not: `metrics` crate fasadı (dış öneri, uygulanmadı)

Dış danışmada gelen alternatif: metrik dışa açımını elle render yerine
**`metrics` crate fasadı + `metrics-exporter-prometheus`** üzerinden
yapmak (Rust'ın fiili metrik standardı; tokio ekosistemi kullanır).

- **Lehine:** battle-tested exporter, ekosistem uyumu, kendi render
  kodunun bakım yükü kalkar.
- **Aleyhine:** yeni bağımlılık zinciri (HTTP feature'ında hyper);
  makrolar global recorder'a yazarak aktör-kanal mimarisinin
  "durum aktörlerde" ilkesinin dışından dolaşır; bütçe-göreli µs
  histogram kenarları gibi özel semantikler yapılandırmaya döner;
  çalışan 215-testlik yüzeyin göçü ~1 gün.

**Karar:** şimdilik uygulanmıyor; gerekirse temiz entegrasyon noktası
**dördüncü lavabo olarak** (`MetricSink::MetricsFacade`) — mevcut
collector hattını atmadan yan yana yaşar. Bu dokümanın konusu
değil, ayrı turda değerlendirilir.
