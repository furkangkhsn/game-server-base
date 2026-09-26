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
| POST | `/rooms/open?id=&tick_hz=` | Runtime oda açma (`ServerHandle.open_room` idempotent-create sözleşmesiyle); oda sunucunun odasıdır (aşağıda) |
| POST | `/rooms/close?id=` | Oda kapatma (`close_room`; persistent ise emeklilik semantiği işler) |

Admin yolları mevcut `ServerHandle` komutlarını kullanır — yeni bir kontrol yolu AÇILMAZ, yalnız transport eklenir.

**`/rooms/open`'ın açtığı oda = sunucunun odası (BACKLOG F8).** Admin
yüzeyi kendi oda varsayılanını UYDURMAZ: açılan odanın `RoomConfig`'i,
başlangıçta ön-kurulan odaların geldiği şablondur
(`config/axes/listeners/room.rs`: `Config::room_template` → tek eşleme;
`Config::room_config(id)` onun üstünde). Oda düzeyindeki bütün anahtarlar
— `tick_hz`, `room_control`, `conn_action`, `max_snapshot_bytes`,
`keepalive_hz`, `max_players`, `max_idle_input_secs`,
`max_detach_hold_secs` — runtime odaya da gider; oyunun `[game]`
tablosu zaten gidiyordu (fabrika, `spawn_registry`'de bir kez kurulur ve
registry her `CreateRoom`'da — başlangıç ya da admin — aynı fabrikayı
çağırır). Eskiden yüzey `RoomConfig::default()` + `tick_hz` kuruyordu:
aynı sunucunun runtime odası başka bir tavanla, başka kapasitelerle
çalışıyordu.

- **İstek başına tek geçersiz kılma `tick_hz`** (verilmezse sunucunun
  hızı; doğrulama aynı: sonlu pozitif sayı değilse 400, registry'ye
  gitmez; global hızı bölmüyorsa registry'nin 400'ü). Başka bir
  geçersiz kılma EKLENMEDİ: oda anahtarları operatörün sunucu çapındaki
  politikasıdır (kapasite, `max_players`, `max_detach_hold` gibi
  güvenlik tavanları) ve kimliksiz v1 yüzeyinin (karar 4) bir güvenlik
  tavanını oda başına gevşetebilmesi istenmez; her ek parametre
  idempotent karşılaştırmaya bir çatışma ekseni daha ekler; query
  grameri bilerek küçük (karar 3). `tick_hz` farklı: odanın hız sınıfı
  gerçek bir oda özelliği (global hızı bölen daha yavaş oda). Oda başına
  farklı ayar isteyen oyun/platform programatik yolu kullanır:
  `ServerHandle::open_room(RoomConfig { …, ..cfg.room_config(id) })`.
- **Sözleşme değişikliği (düzeltmenin sonucu):** durum kodları, hata
  gövdeleri ve idempotentlik kuralı aynı. Değişen yalnız istenen
  config'in kendisi, iki görünür sonucu var: (1) oda anahtarları
  varsayılandan farklı bir sunucuda ön-kurulan bir odayı aynı hızla
  yeniden açmak artık idempotent **200** (eskiden 409 — istek
  `default()` idi, oda sunucunun config'iyle kurulmuştu); (2)
  `keepalive_hz` artık sunucununki, bu yüzden ondan küçük bir `tick_hz`
  isteği registry'nin `KeepaliveRate`'iyle **400** alır (eskiden
  varsayılan 1 Hz'e göre karar verilirdi) — ön-kurulan odalarla aynı
  kural.

## 3. Tel/format detayları

- Metrik adlandırma: `gsb_registry_rooms`, `gsb_room_r1_steps_total`,
  `gsb_conn_frames_in_total` gibi `<alan>_<nesne>_<sayaç>_total`;
  histogramlar Prometheus summary/satır çiftiyle (p50/p99 hazır alanlardan)
- Oda başına (etiket `room="r<id>"`; sharded odada her shard kendi
  satırı, id `room << 16 | index`) küçük pakette (ve W2'de takım
  değişimi için) eklenen sayaç aileleri
  (hepsi kümülatif `counter`; gsb-metric satırında aynı adla, `_total`
  ve `gsb_room_` öneki olmadan):

  | Aile | Anlamı |
  |---|---|
  | `gsb_room_detach_forced_total` | `max_detach_hold` tavanının duran bir `may_release` vetosunu ezerek bitirdiği bekletmeler (`detach_expired_*`'ın alt kümesi; RECONNECT §17) |
  | `gsb_room_effects_applied_total` | Bu shard'ın oyununun otorite olarak uyguladığı uzak etkiler (CROSS-SHARD §4b) |
  | `gsb_room_effects_forwarded_total` | Göç etmiş hedefin yeni sahibine devredilen etkiler |
  | `gsb_room_effects_orphaned_total` | Hedefi artık olmayan etkiler |
  | `gsb_room_effects_dropped_total` | Yolda kaybolan etkiler: dolu yeniden deneme tamponu, kapalı link, hop sınırı, yaş sınırı |
  | `gsb_room_effects_refused_total` | Kaynakta reddedilen `emit`'ler (tick bütçesi bitti ya da hedef ödünç verilmiyor) |
  | `gsb_room_migrations_out_total` | Komşu shard'a devredilen entity'ler (kesinleşen gönderim) |
  | `gsb_room_migrations_in_total` | Komşudan gelip kurulan entity'ler |
  | `gsb_room_migrations_failed_total` | Dolu komşu gelen kutusunun reddettiği göç gönderimleri (sonraki tick yeniden denenir) |
  | `gsb_room_team_exports_total` | Registry'nin takım hub'ına kuyruklanan takım export'ları (CROSS-SHARD §8b; W2) |
  | `gsb_room_team_export_drops_total` | Dolu/kapalı registry posta kutusunun reddettiği export'lar (sonraki tick aynı kümeyi taşır) |
  | `gsb_room_team_export_records_total` | Kuyruklanan export'lardaki kayıtlar |
  | `gsb_room_team_over_cap_total` | Çekirdeğin mesaj başı tavanlarının (`TEAM_EXPORT_MAX_*`) kestiği kayıt/takım — çıkışta ve girişte |
  | `gsb_room_team_imports_total` | Uygulanan takım import'ları (hub'ın buraya ulaşan röleleri) |
  | `gsb_room_team_import_records_total` | Uygulanan import'lardaki kayıtlar |
  | `gsb_room_team_expired_total` | TTL'in düşürdüğü kaynak yuvaları (sessizleşmiş kaynak) |

  Etki, göç ve takım aileleri yalnız shard satırlarında hareket eder
  (tek oda aktörü 0 yazar; takım ailesi yalnız `team_exchange`'i
  uygulayan mantıkta — `ShardedTeamRoom`). Takım sayaçlarının ~1 sn
  penceresi `team_exchange_summary` log satırında da; hub tarafı
  (`relays`, `relay_drops`) `team_hub_summary` satırında kaldı. Crystallization olayları (kit) rapora girmedi — log
  satırı olarak kaldı, gerekçe CROSS-SHARD §4c madde 5.
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
   ve `http_room_config::admin_open_uses_the_server_room_config` — oda anahtarları
   varsayılandan farklı sunucuda ön-kurulan odayı yeniden açmak 200,
   başka hız 409, yeni oda + tekrar 200, geçersiz hız 400 (F8);
   yüzeyin kendi birim testleri (`http/tests.rs`): registry'ye giden
   istek `Config::room_config(id)`'nin alanı alanına aynısı, `tick_hz`
   tek geçersiz kılma, geçersiz hız registry'ye hiç gitmez; uçtan uca
   `mmo_rooms::a_runtime_room_gets_the_server_ceiling`
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
