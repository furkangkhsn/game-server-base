# YENİ OTURUM DEVİR-TESLİM PROMPT'U

> Aşağıdaki metni yeni oturuma olduğu gibi yapıştır. Bu dosya kendisi de
> o oturumun okuyacağı bağlamdır.

---

Sen gsb ("game-server-base") Rust workspace'inde çalışacaksın:
`/home/furkangkhsn/Documents/Projects/Self/game-server-base`. Branch: main.
Mimari skoru 9.0/10, 287 test yeşil, ağaç temiz. Teknik borç listesi
fiilen boş — görevin, sözleşmeli turları devam ettirmek ve disiplini korumak.

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

1. **Çoklu-listener'a QUIC + WS kapıları** — ÜÇ KEZ ajan kaybedilen tur;
   TAZE BAĞLAMDA TEK TUR halinde yap. Sözleşme: ROADMAP "Çoklu-listener"
   maddesinin son paragrafı. `QuicTransport` (gsb-net/src/quic.rs) ve WS
   listener (gsb-net/src/ws.rs) hazır; yalnız `ListenerTransport`'a
   variant eklenip accept görevleri bağlanacak + 2 karışık-transport
   e2e testi.
2. **team × sharded export** — sözleşme: `docs/CROSS-SHARD.md §8`
   (registry-hub BYTE-ENCODED takım-export; RegistryMsg monomorfik
   kalır — generic'e çevirme ELENDİ; TTL sweep + fan-out + izolasyon
   kuralları dahil).
3. **Cross-seam etkileşim paketi** — ROADMAP maddesindeki 3 parça:
   borrowed-view gameplay erişimi · `ShardMsg::RemoteEffect`
   primitifi · histeresizli crystallization tetikleyicisi.
4. Tetikleyicili bekleyenler: NUMA ölçümü (numactl pinli/pinsiz),
   ortak DeltaSnapshotCodec adoptasyonu (all/team/pvs), Ipc/NetLink,
   QUIC rehome.

## BEKLEYEN KULLANICI KARARLARI (kendine sor, tek başına verme)

- Yayın paketi: LICENSE + CI (.github: test/clippy/fmt) + MSRV —
  yarım gün; kullanmaya başlanacaksa ilk iş.
- `metrics` crate fasadına geçiş mi elle render mı (OPS §6 kenar notu).

## DOĞRULAMA TABAN ÇİZGİSİ

Her turdan sonra: `CARGO_HOME=$PWD/.cargo cargo clippy --workspace
--all-targets` → 0 uyarı; `CARGO_HOME=$PWD/.cargo cargo test
--workspace` → tamamen yeşil (bugün itibarıyla 287 passed);
`cargo run --release -p gsb-server --bin gsb-loadgen -- 50 --duration 3`
→ left=50, errors=0, panic yok.
