# YENİ OTURUM DEVİR-TESLİM PROMPT'U

> Aşağıdaki metni yeni oturuma olduğu gibi yapıştır. Bu dosya kendisi de
> o oturumun okuyacağı bağlamdır.

---

Sen gsb ("game-server-base") Rust workspace'inde çalışacaksın:
`/home/furkangkhsn/Documents/Projects/Self/game-server-base`. Branch: main.
294 test yeşil, clippy 0 uyarı, ağaç temiz. Teknik borç listesi fiilen
boş — görevin, sözleşmeli turları devam ettirmek ve disiplini korumak.

**Kaynak ağacı yeniden düzenlendi** (okunabilirlik turu): 40 dosya →
207. Her modül kendi dizini; hedef dosya boyutu 200-250 satır. Bir
dosyayı büyütmek yerine alt modüle böl — ve bir struct'ın impl'ini
bölerken KARDEŞ değil ÇOCUK modül kullan (çocuk atasının private
alanlarını görür, kapsülleme bozulmaz). Hedefi aşan 44 dosya var ve
her biri bilinçli: trait/trait-impl tek blok olmak zorunda, ve tek
sürekli prosedürü ikiye bölmek yarı kurulmuş durumu modül sınırından
geçirmek demek. Yeni bir istisna eklersen commit mesajında gerekçelendir.

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

1. **Yayın paketi** — LICENSE + CI (.github: test/clippy/**fmt**) +
   MSRV. Artık ilk sırada: 207 dosyalık, 44 modül-sınırı görünürlük
   kuralı olan bir ağaç elle koşulan `cargo test` disiplinine
   bağlı kalamaz. `cargo fmt --check` şu an KIRIK (hiç koşulmamış);
   tek mekanik commit'te normalize edip CI'da kapı yap.
2. **WS uyum kapısı** — el yazımı RFC 6455 artık `[[listeners]]`'tan
   erişilebilir, yani servis yolunda. Bilinen açık: fragmentasyon
   sırasında araya giren veri çerçevesi (§5.4) reddedilmiyor
   (`ws/reader/dispatch.rs`, OP_BIN kolu `frag_opcode`'a bakmıyor).
   Tek guard'lık düzeltme + CI'da Autobahn `wstest` kapısı.
3. **team × sharded export** — sözleşme: `docs/CROSS-SHARD.md §8`
   (registry-hub BYTE-ENCODED takım-export; RegistryMsg monomorfik
   kalır — generic'e çevirme ELENDİ; TTL sweep + fan-out + izolasyon
   kuralları dahil).
4. **Cross-seam etkileşim paketi** — ROADMAP maddesindeki 3 parça:
   borrowed-view gameplay erişimi · `ShardMsg::RemoteEffect`
   primitifi · histeresizli crystallization tetikleyicisi.
5. Tetikleyicili bekleyenler: NUMA ölçümü (numactl pinli/pinsiz),
   ortak DeltaSnapshotCodec adoptasyonu (all/team/pvs), Ipc/NetLink,
   QUIC rehome.

## BEKLEYEN KULLANICI KARARLARI (kendine sor, tek başına verme)

- `metrics` crate fasadına geçiş mi elle render mı (OPS §6 kenar notu).

## DOĞRULAMA TABAN ÇİZGİSİ

Her turdan sonra: `CARGO_HOME=$PWD/.cargo cargo clippy --workspace
--all-targets` → 0 uyarı; `CARGO_HOME=$PWD/.cargo cargo test
--workspace` → tamamen yeşil (bugün itibarıyla 294 passed);
`cargo run --release -p gsb-server --bin gsb-loadgen -- 50 --duration 3`
→ left=50, errors=0, panic yok.
