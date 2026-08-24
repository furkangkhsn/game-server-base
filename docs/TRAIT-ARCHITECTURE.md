# gsb: Trait Mimarisi — Birleşim ve PlayerId Yolu

> Durum: TASARIM (Faz 1 uygulama onaylandı). RECONNECT.md gibi bu
> doküman da uygulamanın sözleşmesidir; §6'daki faz tablosu sırayı
> sabitler.

## 1. Amaç

"Yeni oyun = tek mantık yazmak" vaadini tamamlamak. Bugün bir oyun
sharded çalışacaksa **iki** neredeyse-aynı trait implement ediyor
(~17 ortak metot kopya) ve shard tarafı RPC / match-result / keepalive
yeteneklerinden yoksun yaşıyor. Ayrıca resume'un RebindKey'i
(RECONNECT §14.1) oda içi anahtarların oturum-bağımsızlaşmasıyla
kökten ortadan kalkabilir.

## 2. Mevcut yüzey (ölçülmüş)

| Grup | Üyeler |
|---|---|
| **Ortak (~17)** | `GroupKey`, `snapshot_op`, `private_op`, `group_of`, `snapshot`, `private`, `on_join`, `on_leave`, `on_disconnect`, `may_release`, `on_detach_expired`, `resume_lookup`, `on_resume`, `ingest`, `update`, `on_shutdown`, `encoded_records` |
| **Yalnız Room** | `handle_request` (RPC), `match_result`, `keepalive` |
| **Yalnız Shard** | `State`, `index`, `shard_count`, `serial_base/range/used`, `neighbors`, `collect_migrations`, `on_migrate_in/out`, `collect_border`, `own_wires` |

Shard'ın eksikleri tesadüf değil: RPC pending-set/sweep'i, result-sink
ve keepalive kadansı **aktör makinesi** gerektirir. Trait kopyası ile
yetenek boşluğu birbirine dolanmıştır; bu tasarım ikisini ayırır.

## 3. Adaylar

### A. Ortak supertrait + iki uzantı — KABUL ✅

```
GameLogic<W>                     ← 17 ortak metot (tek kaynak)
├─ RoomLogic<W>:  GameLogic<W>   + handle_request, match_result, keepalive
└─ ShardLogic<W>: GameLogic<W>   + topoloji hook'ları
      (State, index, shard_count, serial_*, neighbors,
       collect_migrations, on_migrate_in/out, collect_border, own_wires)
```

Derleme zamanı ayrım korunur: `neighbors()`/serial aralığı sharded
oyun için **zorunluluğunu kaybetmez** — unutulursa derlenmez, sessiz
migrasyon kırılımı yapısal olarak imkânsızdır.

### B. Tek unified trait — ELENDİ ❌

Shard hook'ları default-boş olurdu; sharded oyun `neighbors()`'ı
unutup derlenirdi ve migrasyon sessizce yanlış çalışırdı. "Tek trait"
estetiği, doğruluk garantilerinden pahalıydı.

### C. Macro üretimi — ELENDİ ❌

Kullanıcı hatalarında okunabilirlik/doküman deneyimi zayıflar;
başkaslarının üzerine inşa edeceği bir base için yanlış üslup.

## 4. Yetenek terfisi — fazlama

GameLogic'e `handle_request`/`match_result` koymak shard'a *imkânı*
verir; *çalışması* için shard actor'üne pending-set, timeout sweep ve
result-sink makinesi de gerekir. Bu iki ayrı iştir:

- **Faz 1 (bu tur):** birleşim + `keepalive` terfisi. Keepalive ucuzdur:
  oda tarafındaki sözleşme ("değişmediyse cache'ten tam snapshot'ı
  yeniden gönder", `RoomLogic::keepalive`) shard broadcast'ının mevcut
  grup-cache yapısına doğrudan oturur; shard actor kadans sayacını
  odadaki kalıptan alır. Sharded oyun böylece ilk gerçek yeteneğini
  kazanır.
- **Faz 2 (ayrı tur):** shard-RPC (pending set + sweep + private-frame
  yanıtı) ve match-result sink'i shard actor'üne taşıma. Oda
  aktöründeki emsaller: `crate::rpc` + result seam.

## 5. PlayerId geçişi (Faz 2 sonrası ayrı tur)

- **PlayerId(u64)** yeni tip: ilk join'de **logic tarafından**
  mintlenir (kimlik politikası oyunun işidir; park defteri zaten
  identity ↔ durum eşlemesini tutar — oraya yerleşir).
- ConnectionId transport-oturum anahtarı olarak KALIR (conn actor ↔
  registry ↔ kanal adresleme). Oda içi tablolar (`conns`, `roster`,
  `pending`, `queued`, grup listeleri) PlayerId'e geçer; conn ↔ PlayerId
  bağlamı TEK bağlama tablosunda yaşar.
- Resume/bot/rebind artık tüm tablolarda RebindKey değil, bağlama
  satırının güncellenmesidir (RECONNECT §14.1'in kökten çözümü).
- `WireId` olduğu gibi kalır: entity kimliği wire sözleşmesidir;
  PlayerId oyuncu kimliğidir. İleride oyuncu-çok-entity senaryosuna
  kapı açıktır.
- Sıralama: FAZ 1'den SONRA — aksi halde re-key değişikliği iki kopyada
  yapılır.

## 6. Faz tablosu

| Faz | İçerik | Durum |
|---|---|---|
| 1 | `GameLogic` supertrait + Room/Shard uzantıları; keepalive terfisi (shard actor kadansı dahil); gsb-game impl'lerinin bölünmesi | BU TUR |
| 2 | PlayerId + bağlama tablosu; RebindKey küçültmesi | ✅ Kapatıldı — tablolar PlayerId'e geçti, resume tek bağlama satırı güncelliyor (§5 tamamlandı); `pending`/`queued` ve shard epoch/tombstone tabloları BY DESIGN conn-anahtarlı kaldı (oturum-kapsamlı; kod içi signpost'larda CHECKED) |
| 3 | Shard-RPC + match-result makinesi | Planlı |

## 7. Test stratejisi

- Mevcut 193 testin tamamı yeşil kalır (davranış değişmez; yalnız yüzey
  taşınır) — suite, birleşimin gerçekte "kopya taşıma" olduğunu kanıtır.
- Keepalive terfisine yeni davranış kilidi: sharded odada sessiz grup,
  keepalive kadansında cache'ten tam snapshot alır (oda tarafındaki
  `keepalive_above_tick`/cache testlerinin shard karşılığı).
- Faz 2/3 kendi kilitlerini kendi turlarında ekler.

## 8. Bilinçli olarak yapılmayanlar

- Tek unified trait (B) ve macro üretimi (C) — bkz. §3.
- Shard RPC/match-result — Faz 3.
- PlayerId — Faz 2 (bu turda yalnız plan).
