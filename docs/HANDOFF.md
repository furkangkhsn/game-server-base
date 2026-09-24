# YENİ OTURUM DEVİR-TESLİM PROMPT'U

> Aşağıdaki metni yeni oturuma olduğu gibi yapıştır. Bu dosya kendisi de
> o oturumun okuyacağı bağlamdır.

---

Sen gsb ("game-server-base") Rust workspace'inde çalışacaksın:
`/home/furkangkhsn/Documents/Projects/Self/game-server-base`. Branch: main.
411 test yeşil, clippy 0 uyarı, ağaç temiz. Görevin, sözleşmeli turları
devam ettirmek ve disiplini korumak.

Teknik borç turu (CHANGELOG "teknik borç turu") üç borcu kapattı ve
**dört maddeyi ürün kararına bıraktı**. Bunlardan **ikisi kapandı**
(CHANGELOG "bağlantı sınırları turu"): tıkanmış yazmaya süre sınırı
(`write_stall_secs`, ilerleme tabanlı, vars. 10 sn) ve post-auth
HEARTBEAT_ACK kısması (§3.2 eşiği auth sınırının ötesine taşındı).
**Üçüncüsü de kapandı** (CHANGELOG "AFK sinyali + girdi-boşta tavanı
turu"): AFK artık base'in kararı DEĞİL — base koşulsuz bir SİNYAL
yayınlıyor (`TickCtx::since_input`, "aksiyon taşıyan kare" yapısal
tanımı) ve varsayılan KAPALI bir TAVAN sunuyor
(`max_idle_input_secs`); tavan dolduğunda kararı oyunun
`on_disconnect`'i veriyor, base kendiliğinden despawn etmiyor. Detach /
park / bot etkileşimi: `docs/RECONNECT.md` §16.

**Bir madde hâlâ açık** — geçerli girdiye hacim limiti. Bir oynanış
parametresi seçmeyi ister (mevcut bir mekanizmayı simetrik tamamlamayı
değil); tek başına verme, ROADMAP'teki gerekçeleri oku.

O turun açık yan bulgusu (`step_fine_hist` shard aktöründe hiç
yazılmıyor) **kapandı** — bkz. CHANGELOG "park sızıntısı + shard metrik
boşluğu turu". Aynı tur, bir öncekinin yarım kalan park sızıntısını da
kapattı: registry satırı artık politika park etmeyi REDDETTİĞİNDE de
bırakılıyor (`disconnect_grace_secs = 0`, yani varsayılan, tam olarak bu
koldur); `RegistryMsg::ParkExpired` → `DetachDespawned`.

Onun yerine geçen açık yan bulgu (`step_min_us` / `late_min_us`
minimum değil) da **kapandı** — bkz. CHANGELOG "minimum sayaçlar turu".
Kullanıcı kararı ONARIM oldu: alanlar gerçek minimum yapıldı,
`step_first_us` yeniden adlandırması elendi. Muhasebe iki aktörden
`RoomCounters::observe_late_us`/`observe_step_us` çocuk modülüne alındı —
yeni bir süre sayacı eklerken oraya ekle, aktörlerin `lifecycle.rs`'ine
değil. İlk gözlemin iki ucu da SEED etmesi bir tuzak koruması: sıfırdan
başlayan bir minimum sonsuza dek 0 kalır.

Onun yerine geçen yan bulgu (`fold_rooms` minimumlar DIŞINDA da eksik
katlıyor) da **kapandı** — bkz. CHANGELOG "metrik fold denetimi turu".
Artık her alanın katlama kuralı kararlaştırılmış, koda yazılmış ve
DESIGN §12'ye işlenmiştir; kural YAPISALDIR: fold döngüsü
`RoomReport`'u tam destructure eder, yani rapora alan eklemek kuralı
yazılana kadar **derlemez** (E0027). Yeni bir metrik alanı eklerken üç
yer seni zaten derlemeyi kırarak uyarır: iki aktörün `sample()`'ı,
toplayıcının `RoomReport` literal'i ve `fold_rooms`'un destructure'ı.
Denetimin iki yan bulgusu da kapandı: akümülatör shard 0'ı HER toplamda
iki kez sayıyordu (50 istemcilik sharded koşu `members=61` diyordu) ve
loadgen'in ince-histogram percentilleri yanlış nüfusa soruluyordu
(`steps` MAX, histogramlar SUM ile katlanır → `folded_steps`).
**Açık yan bulgu kalmadı.**

Sonraki tur (CHANGELOG "stall gözlemlenebilirliği + bayt-granüler
ilerleme turu"): sunucunun başlattığı her kapanış artık sebebiyle
sayılıyor (`ServerClose`, `gsb_net_server_closes_total{reason}`,
loadgen `server_closes=`) ve write-stall saati kare değil BAYT ölçüyor.
Yeni bir sunucu-kapanış yolu eklersen ona bir `ServerClose` sebebi ver
(`ConnIn::ServerClosed { cause, .. }`) — istemci-tarafı son ve shutdown
bilerek sayılmaz. Bekleyen: 10k A/B ölçümü (`1c22c99` ↔ `6f3d8f5`).

**Kaynak ağacı yeniden düzenlendi** (okunabilirlik turu): 40 dosya →
207. Her modül kendi dizini; hedef dosya boyutu 200-250 satır. Bir
dosyayı büyütmek yerine alt modüle böl — ve bir struct'ın impl'ini
bölerken KARDEŞ değil ÇOCUK modül kullan (çocuk atasının private
alanlarını görür, kapsülleme bozulmaz). Hedefi aşan 44 dosya var ve
her biri bilinçli: trait/trait-impl tek blok olmak zorunda, ve tek
sürekli prosedürü ikiye bölmek yarı kurulmuş durumu modül sınırından
geçirmek demek. Yeni bir istisna eklersen commit mesajında gerekçelendir.

**gsb-kit tasarımı onaylandı ve Faz 0 tamam** (`docs/KIT-ARCHITECTURE.md`
§10, CHANGELOG "gsb-kit Faz 0 turu"): `gsb-game/src` artık `kit/`
(stratejiler + ortak makine) ve `demo/` (örnek oyun). Kit kodu demo'ya
YALNIZ `kit/seam.rs` üzerinden erişir — `kit/` altında seam dışında her
`crate::` yolu `kit::` ile devam etmeli, `src/layering.rs` testi bunu
kırar. Kit'e demo bağımlılığı eklemen gerekirse seam'e, hedef seam
grubuna, tek satırlık gerekçeyle ekle. Seam'in içeriği Faz 1'in iş
listesidir; Faz 1 bitince seam boşalır ve silinir.

## ÖNCE OKU (sırayla)

1. `README.md`
2. `docs/ROADMAP.md` — başındaki **DEVAM NOTU** zorunludur (ajan kaybı
   dersleri + ortam notları).
3. Aşağıdaki iş sırasına göre ilgili turun sözleşme dokümanı.

## ZORUNLU DİSİPLİN (istisnasız)

- `crates/gsb-lint` build'i kırar: kaynakta `tokio::select`,
  `futures::select`, `select!`, `Mutex`, `RwLock`, `parking_lot`
  geçMESİN (yorumlar sökülür ama STRING LITERAL KORUNUR — bu kelimeleri
  string'e bile yazma!).
- Aktörler tek-awaited; kanallar bounded; hot path `try_send/try_recv`.
- Çalışma döngüsü: iddiayı doğrula → düzelt → davranış-kilitleyici test
  (mümkünse mutation-check) → clippy 0 uyarı → tüm süit yeşil → kommit.
- Yeni bağımlılık gerekiyorsa cargo komutlarının başına
  `CARGO_HOME=$PWD/.cargo` ekle (HOME önbelleği salt-okunur olabilir).
- **Ajan aktifken asla `git add -A`** — yalnız açık pathspec; ya da ajan
  bitene kadar bekle. (Bu oturumda iki kez pahalıya patladı.)
- Paralel ajan çalıştıracaksan `git worktree` kullan (aynı ağaçta iki
  ajan = dosya çakışması; bu oturumda bir kez oldu).
- Her turda: ROADMAP durum satırı güncellenir, elenen alternatifler
  belgelenir, rapor iddiaları parent tarafından koddan doğrulanır.
- Doküman geleneği: tasarım kararı vermeden önce ELENEN ALTERNATİFLER
  yazılır; tetikleyicisiz optimizasyon yapılmaz ("önce veri").

## SABİT MİMARİ KARARLAR (yeniden tartışma — hepsi ölçüm/kararla sabit)

- rUDP deneysel statüde; REL/RAW band kuralı: opcode ≤64 güvenilir,
  ≥1000 kayıp-toleranslı. Band seçimi BOYUT değil KAYBIN BEDELİ
  (DESIGN §5.1).
- Delta border exchange main'de (A/B: 0.39× byte); Faz C sonrası
  aynı-process komşular AlwaysFull (lokal CPU kıt).
- Kalıcılık iki sınıf: maç oyunları = her-tick typed CLONE checkpoint
  (encode ASLA — checkpoint process'i terk etmez); MMO = event-based +
  periodic checkpoint + logout flush; otorite merkezi katmanda, game
  server cache'tir. Detay: `docs/PERSISTENCE.md`.
- Üç eksenli config: `topology × visibility × communication`;
  desteklenmeyen kombinolar startup'ta faz-bilgili hata ile reddedilir.
- `DemoRoom` artık `OpenRoom`; `BorrowedRecord` artık
  `BorderRecord<Strip>` (core zarf + logic payload); shard↔komşu
  haberleşmesi `ShardLink` trait'i arkasında.

## İŞ SIRASI (sözleşmeli turlar)

1. ~~**Yayın paketi**~~ — **KAPANDI** (CHANGELOG "yayın paketi turu"):
   MIT `LICENSE`, MSRV = sabit toolchain = 1.95.0 (alt sınır
   `bevy_ecs 0.19.1`), `.github/workflows/ci.yml` (fmt check · clippy
   `-D warnings` · test), `CONTRIBUTING.md`. (Build artık sistem
   `protoc`'u İSTEMİYOR — protokol sertleştirme turu gömülü protoc'u
   bağladı, CI'daki `protobuf-compiler` adımları kaldırıldı.)
   `cargo fmt --all --check` temiz — artık CI kapısı; yeni kod
   formatlanmış gelmeli.
2. ~~**Protokol sertleştirme**~~ — **KAPANDI** (CHANGELOG "protokol
   sertleştirme turu"): RPC yanıt zarfı base'e taşındı (DESIGN §5.2),
   `reserved` disiplini kuruldu (§5.3), ERROR kodları üretilen enum
   oldu (§5.4) ve protokol sürümü AUTH'a eklendi (§5.5). Tel
   değişmedi. Yeni kod yazarken: `ERROR` üretmenin tek yolu
   `base::Error::new(ErrorCode::…, msg)`; yeni bir `CoreError`/
   `ProtoError` varyantı eklemek `wire_code` eşlemesini DERLEMEZ hale
   getirir (kasıtlı); `.proto`'dan alan silersen aynı commit'te
   numarasını VE adını `reserved` et, commit'i gerekçe olarak an.
   Açık kalan tek parça sürüm ARALIĞI politikası — tetikleyicisi
   ROADMAP'te ("Protokol sözleşmesi" bölümü).
3. **WS uyum kapısı** — el yazımı RFC 6455 artık `[[listeners]]`'tan
   erişilebilir, yani servis yolunda. Bilinen açık: fragmentasyon
   sırasında araya giren veri çerçevesi (§5.4) reddedilmiyor
   (`ws/reader/dispatch.rs`, OP_BIN kolu `frag_opcode`'a bakmıyor).
   Tek guard'lık düzeltme + CI'da Autobahn `wstest` kapısı.
4. **team × sharded export** — sözleşme: `docs/CROSS-SHARD.md §8`
   (registry-hub BYTE-ENCODED takım-export; RegistryMsg monomorfik
   kalır — generic'e çevirme ELENDİ; TTL sweep + fan-out + izolasyon
   kuralları dahil).
5. **Cross-seam etkileşim paketi** — ROADMAP maddesindeki 3 parça:
   borrowed-view gameplay erişimi · `ShardMsg::RemoteEffect`
   primitifi · histeresizli crystallization tetikleyicisi.
6. Tetikleyicili bekleyenler: NUMA ölçümü (numactl pinli/pinsiz),
   ortak DeltaSnapshotCodec adoptasyonu (all/team/pvs), Ipc/NetLink,
   QUIC rehome.

## BEKLEYEN KULLANICI KARARLARI (kendine sor, tek başına verme)

- `metrics` crate fasadına geçiş mi elle render mı (OPS §6 kenar notu).

## DOĞRULAMA TABAN ÇİZGİSİ

Her turdan sonra: `cargo fmt --all --check` → temiz;
`CARGO_HOME=$PWD/.cargo cargo clippy --workspace
--all-targets -- -D warnings` → 0 uyarı; `CARGO_HOME=$PWD/.cargo cargo test
--workspace` → tamamen yeşil (bugün itibarıyla 411 passed);
`cargo run --release -p gsb-server --bin gsb-loadgen -- 50 --duration 3`
→ left=50, errors=0, panic yok.
