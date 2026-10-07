# gsb: Servis Çağrıları ve İstekler — Tasarım (tur s0b)

> **Faz 1 uygulandı (tur q1):** `gsb-core::calls` (sözleşme, `Calls`,
> çağrı oluşturucu, motor fazları, asgari süreç içi host `calls::serve`),
> oda ve shard aktörlerinde tek ortak istek kodu (`room::requests::Parts`),
> `GameLogic::{boot_calls, update_with_calls, call_owner, owner_alive}` +
> `ShardLogic::update_seam_with_calls`, `CallCounts` sayaçları
> (gsb-metric, Prometheus, OTLP, loadgen teli). Tasarımdan sapmalar §20
> "Faz 1 — uygulanan biçim"de.
>
> **Faz 2 uygulandı (tur q2):** `gsb-macros` + `#[gsb_core::service]`
> (çağrı tipleri, posta kutusu ve çıplak mesaj, tower yönlendiricisi,
> `…Client`, `…Handle` — `within`/`on_behalf_of` —, `X::serve`),
> `gsb_core::calls::{spawn, Services, Setup}` tower hostu (katman hataları
> tipli sona, varsayılan 30 sn süre, `gsb.call` span'i + metot başına
> gecikme), `RegistryParts::service(X::serve(..), |s| ..)` /
> `parts.client()` (F5: odalardan sonra durur), tablonun kendi süre
> süpürmesi (`RoomConfig::call_timeout`), kit kancaları, `BootError`'da
> oda kurulmaz (`CoreError::RoomBoot`), motor aralığı op'u
> (`BootError::EngineOp`), `call_share`/`call_completions`/`call_timeout_secs`
> sunucu config'i, demo ekonomisi `trait Economy`. Sapmalar §20 "Faz 2 —
> uygulanan biçim"de.
>
> **Faz 4 uygulandı (tur q4):** `gsb_core::determinism` — `TickCtx::{time,
> rng}` (oda saati = `dt`'lerin toplamı; tohumlu SplitMix64, `RoomConfig::seed`
> ya da enkarnasyon başına taze tohum), tohumdan türeyen çağrı anahtarı
> kökeni, `GameLogic::state_hash`, TEST modu (`Sim` + `Delay` + `SimRoom`),
> `record` özelliği (oda başına girdi kaydı, çerçeveli dosya, `Replay`) ve
> `replay_diverged`/`replay_undecodable`/`records_written`/`records_dropped`
> sayaçları. Sapmalar §20 "Faz 4 — uygulanan biçim"de.
>
> **Faz 5 uygulandı (tur q5):** ECS başına bir bağlayıcı crate —
> `gsb-bevy-ecs` (`calls`: `GsbCalls` kaynağı + `lend`, `Svc<T>`,
> `OwnerExt`/`EntityExt`, `owner_alive`) ve `gsb-hecs` (`calls`: uzantılar
> + `owner_alive`); shipyard yalnız belge; `gsb-conformance` (yayınlanmaz)
> aynı senaryonun ECS'siz, bevy_ecs ve hecs biçimlerinde aynı izi verdiğini
> gerçek oda aktörüne karşı kanıtlar. Shard göçü çağrıları taşır
> (`PlayerMigration.calls`, B152); açık `calls.bind` park + devamdan sağ
> çıkar (B156, kayıt/tekrar dahil); boş `Reject` gerekçesi
> `REJECTED_REASON` (B163); kit seam yolu gerçek shard aktörleriyle
> sınandı (B168); yinelenen servis kaydı `RegisterError` (B169).
> Sapmalar §20 "Faz 5 — uygulanan biçim"de.
>
> **Durum:** TASARIM — kullanıcı incelemesi bekliyor. Uygulama YOK; bu
> turun çıktısı bu doküman ve `spikes/s0-calls` (gsb çalışma alanının
> üyesi olmayan, motoru değiştirmeyen bir kanıt crate'i + `#[service]`
> proc-macro alt crate'i). s0'ın istek/çağrı kısmı, kullanıcıyla yapılan
> uzun tasarım tartışmasından sonra baştan yazıldı (§2'deki kararlar
> yeniden açılmaz); açık kalanlar §19'da numaralı sorular olarak.
> **Takip (s0b-2):** kullanıcı S1 sapması (`serve(..)`), S2–S5 önerileri,
> `forward` kapanışı, `.answers(r.reply)` ve "ret bir sonraki tick" kararını
> onayladı. İki değişiklik: (1) oturum → sahip bağlama (`calls.bind`,
> `Incoming.owner`; S14 kapandı), (2) **çekirdeğin hiçbir ECS bağımlılığı
> yok**; ECS entegrasyonu çekirdeğin dışındaki isteğe bağlı adaptör
> crate'lerinde (S3 kapandı).
>
> **Kapsam:** İSTEKLER (servis çağrıları + istemci istekleri). OLAYLAR
> (sunucunun ittiği) bu turun dışında — s0 metni §15'te duruyor,
> **sonraki tur, yeniden tasarlanacak**.
>
> Kod ve yorumlar İngilizce'dir; bu doküman Türkçe'dir (repo sözleşmesi).

**Tek paragraf özet.** Oyun servislerini **düz bir async trait** olarak
yazar (`#[gsb::service] trait Inventory { async fn craft(..) -> Result<..> }`);
makro tipli çağrıları, tower üstünde koşan servis tarafını, odalar için
**beklemeyen** bir istemciyi (`InventoryClient`) ve servisler/admin için
**beklenebilir** bir istemciyi (`InventoryHandle`) üretir. İstemcinin
(oyuncunun) yolladığı istekler `.proto` `service` bloklarından gelir.
Her tick aktörünün (oda, zamanlayıcı, admin görevi) tek bir **`Calls`
tablosu** vardır ve ECS'ten bağımsızdır: istekler tablodan **alınır**
(`take_incoming`), çağrılar tabloya **yazılır**
(`calls.call(inv.craft(..)).owner(o).with(ctx).answers(req)`), sonuçlar
tablodan **okunur** (`take_resolved`). Çağrı hiç beklemez ve hiç
yerinde başarısız olmaz — ret de dahil her son aynı yoldan, bir sonraki
tick'in RESPONSES fazında gelir. Kimsenin almadığı/cevaplamadığı şey
tick sonunda motor tarafından kapatılır ve **adıyla sayılır**. Oda
determinizm sınırıdır: tick'e giren her şey kanonik sırayla kaydedilip
REPLAY modunda servis çağırmadan aynen tekrar oynatılabilir.

```text
CONTROL → RESPONSES → READ → (EVENTS, sonraki tur) → CONVERT → SYSTEMS → BROADCAST
```

---

## 1. Problem (s0'dan, kısaltılmış)

Bugün tick dışı iş için tek dikiş `GameLogic::handle_request` →
`RequestDecision::External(future)` (RPC-CONTROL-PLANE §1–3). Üç sınırı:

1. **Yalnız istemci isteği tetikleyebilir.** Bir sistem ("zindan bitti →
   ödül yaz", "her 30 sn kaydet") servis çağıramaz; her oyun kendi
   `tokio::spawn` + kanalını yeniden icat eder, sayaçsız.
2. **Sonuç dünyaya dönmüyor**, doğrudan istemciye bayt olarak gidiyor:
   rezerv/onay ("altını ayır → servis onaylasın → düş") yazılamaz.
3. **Kullanım temizliği yok:** demo ekonomisi (`gsb-demo/src/demo/economy.rs`)
   elle yazılmış mesaj enum'u, oneshot ve bayt dönüşümüyle ~160 satır —
   her servis için aynı tören.

s0 bunları `ctx.call(&svc, req, tag)` + `Done<Tag>` + hub ile çözüyordu;
kullanıcıyla tartışmada üç şey ağır bulundu: etiket kapanışları ve
`Done` enum'u (her oyun bir "bitti" enum'u + eşleme yazıyor), ECS
sistemlerinin fiile ulaşamaması (`SystemCtx::ext` yaması) ve hub (ikinci
bir oda türü + ikinci bir cevap yolu). s0b bunların üçünü de kaldırır.

## 2. Kararlar (kullanıcıyla alındı — yeniden açılmaz)

| # | Karar | Gerekçe (özet) |
|---|---|---|
| 1 | **İki tanım kaynağı.** Süreç içi servisler: `#[gsb::service]` ile işaretli Rust trait'i (makro: metot başına çağrı tipi + `impl Call`, tower tabanlı yönlendirici, beklemeyen `XClient`, beklenebilir `XHandle`). İstemci-tel istekleri: `.proto` `service` blokları, `option (gsb.op) = N` (+ hata tipi) → üretilmiş `Request` impl'leri (ileride C#/TS/GDScript istemcileri). Trait ayrımı: `Call { Response; Error }` (süreç içi, op yok, tel yok) ve `Request: Call + WireMessage { const OP }`. Arayan kimliği istemci yükünden ASLA gelmez. | Oyun geliştiricinin kullanım temizliği birinci öncelik; makro tekrarı kaldırır. Süreç içi tipler protobuf olmak zorunda değil. |
| 2 | **Çekirdekte ECS'ten bağımsız `Calls` tablosu** (bileşen değil, kapanış/devam değil, `Done` enum'u değil, bileşende yoklanan handle değil): `take_incoming`, `take_incoming_all`, `call(..).owner().with().answers()`, `take_resolved`, `in_flight`, `forward`, `export`/`import`. Owner opak `u64`; bevy_ecs/hecs yalnız minik bir `.entity()` uzantısı alır. bevy_ecs'te tablo şema boyunca bir `Resource` (adaptörün `GsbCalls` yeni tipi); hecs/ECS'siz oyunda `&mut Calls` geçilir. **Takip kararı: çekirdeğin hiçbir ECS bağımlılığı yoktur** (isteğe bağlı özellik arkasında bile); adaptörler ayrı, isteğe bağlı crate'lerdir. Söz konusu olan yalnız `bevy_ecs` crate'idir — Bevy oyun motoru asla. Alınmayan sonuç ve cevaplanmayan istek tick sonunda kapatılır ve sayılır. | Tek kavram, her ECS'te aynı; "her kaybı say". |
| 3 | **Servisler cevaplar; çağıranlar yalnız okur ve temizler.** `Calls` tablosu + boşaltma noktası olan her aktör çağırabilir (oda bir türü; zamanlayıcı/admin görevi bir diğeri). **Hub YOK**: lobi bir odadır; her oyuncu her zaman bir odadadır ("ECS'te her şey birer oda"). | Tek programlama modeli, tek cevap yolu. |
| 4 | Tick: **CONTROL → RESPONSES → READ → (EVENTS) → CONVERT → SYSTEMS → BROADCAST.** RESPONSES servis tamamlanmalarını READ'den önce tabloya boşaltır. | Girdi, dünyanın en güncel hâline karşı doğrulanır (s0 gerekçesi). |
| 5 | **Determinizm** gerekli: geçmiş maçın hata ayıklama tekrarı, hile doğrulama (tekrar oynat + karşılaştır), izleme tekrarı, deterministik testler. Canlı zamanlama deterministik değildir; tekrar üretilebilirlik oda sınırında **kayıttan** gelir. `ctx.time()/dt()`, tohumlu `ctx.rng()`, deterministik çağrı anahtarları, opt-in girdi kaydı, REPLAY modu, deterministik TEST modu (`Delay::ticks(n, impl)` / `Delay::never`), tick başına durum hash kancası. Oda determinizm sınırıdır; oda dışı çağıranlar kaydedilmez. | Hata ayıklama ve hile doğrulama aynı makineyi ister. |
| 6 | Sürümleme: op'lar asla yeniden kullanılmaz; çakışma boot'ta yakalanır; bilinmeyen op → motor "no handler". Gözlemlenebilirlik: çağrı anahtarı trace id taşır; servis katmanı metot başına span + gecikme yayar (OTLP ihracatçısı var). | |
| 7 | Oyunun işi (motorun değil): yetkilendirme, istek başına hız sınırı, sayfalama. | "Motor, oyun değil." |
| 8 | FFI yalnız istemcide (C ABI yalnız C# için; Godot gdext, JS wasm-bindgen Rust çekirdeğini doğrudan kullanır). İstek id'sini sarmalayıcı atar; iptal için `Command::Forget{id}`; tampon küçükse `poll` gereken boyutu döner; tipli hatalar `game_error` ile. `unsafe` yalnız `gsb-ffi` crate'inde. | |

## 3. Tick

```text
global ticker ──TickInfo──▶ oda aktörü (tek await)
   0  CONTROL    join/leave/shutdown (değişmedi)
   1  RESPONSES  önceki FLUSH'ın retleri + tamamlanma kanalı (try_recv);
                 her sonucu bekleyen girişle eşle; `.answers` bağlıysa
                 istemci cevabını hemen yaz; sahibi yaşıyorsa sonucu
                 türüne göre kovaya koy (take_resolved bekler)
   2  READ       üye kanallarından istekler → tablo: çözülür, tipine göre
                 kuyruğa (ya da `forward` kuralıyla doğrudan servise)
  (3  EVENTS     sonraki tur)
   4  CONVERT    oyunun ilk sistemleri: take_incoming / aksiyonlar
   5  SYSTEMS    oyun; calls.call(..) yalnız tabloya yazar
      FLUSH      bu tick'in çağrıları sırayla gönderilir (try_send);
                 ret burada kararlaşır, bir SONRAKİ RESPONSES'ta teslim
   6  BROADCAST  tick sonu: alınmayan istek / cevaplanmayan istek /
                 alınmayan sonuç kapatılır + sayılır; cevaplar gönderilir
```

**Neden RESPONSES READ'den önce** (s0'dan, değişmedi): bu tick'in
girdisi en güncel dünyaya karşı doğrulanır ("500 altın geldi" sonucu ile
500'lük satın alma aynı tick'te ise satın alma kabul edilir); RESPONSES'ta
verilen istemci cevabı korelasyon id'sini aynı tick'in READ'i için
serbest bırakır.

**Boşta maliyet (spike `idle_cost`):** sessiz bir odada RESPONSES = tek
boş `try_recv`, READ = tek boş `try_recv`. Tablonun tick başına
tamponları (`Vec::new()`/`HashMap::new()`) hiç ayrılmaz — 1000 boş
tick sonra altı tamponun kapasitesi 0, hiçbir sayaç oynamaz. FLUSH ve
tick sonu kapatma boş tamponda tek `is_empty` kontrolüdür.

## 4. İki tanım kaynağı

### 4.1 Süreç içi servis: `#[gsb::service]`

Oyunun yazdığı:

```rust
#[gsb::service]
pub trait Inventory {
    async fn load(&self, cx: &CallCx, who: Identity) -> Result<InventoryData, InvError>;
    async fn grant(&self, cx: &CallCx, who: Identity, item: ItemId) -> Result<PickedUp, InvError>;
    async fn craft(&self, cx: &CallCx, recipe: RecipeId, consume: Vec<ItemId>) -> Result<Crafted, InvError>;
    async fn save(&self, cx: &CallCx, who: Identity, items: Vec<ItemId>, equipped: ItemId) -> Result<(), InvError>;
}

pub struct PgInventory { db: PgPool }

impl Inventory for PgInventory {
    async fn craft(&self, cx: &CallCx, recipe: RecipeId, consume: Vec<ItemId>) -> Result<Crafted, InvError> {
        let who = cx.player().ok_or(InvError::NoPlayer)?;     // doğrulanmış kimlik, yükten değil
        // cx.key() = idempotency anahtarı: aynı anahtar → aynı sonuç, tek etki
        self.db.craft_once(cx.key(), who, recipe, &consume).await
    }
    // load, grant, save …
}

// kompozisyon kökü
server.service(Inventory::serve(PgInventory::new(db)), |s| s.timeout(Duration::from_secs(3)));
let inv: InventoryClient = server.client();     // odalara
let inv_h: InventoryHandle = server.client();   // servislere / admin'e
```

(`Inventory::serve(..)` sarmalaması onaylanan biçimden tek sapma —
yetim kuralı; §19 S1.)

Makronun ürettiği (spike `macros/src/lib.rs`, hepsi gerçekten
derleniyor):

| Üretilen | Ne işe yarar |
|---|---|
| `mod inventory { pub struct Load { pub who }, Grant, Craft, Save }` + her biri için `impl Call` | Metot başına bir çağrı tipi; `take_resolved::<inventory::Craft, _>()` bununla seçer. |
| `inventory::Msg` (posta kutusu enum'u), `inventory::Kind` (işaretçi: `ServiceKind` + her `S: Inventory` için `ServiceDef<S>`) | Elle yazılan mesaj enum'u + `From` impl'lerinin yerini alır. |
| `inventory::Router<S>`: `tower::Service<Dispatch<Msg>>` | Servis tarafı tower'da koşar; oyunun katmanları (timeout, concurrency_limit, load_shed, kendi katmanı) bunun üstüne dizilir. |
| `InventoryClient` (Clone) | **Beklemez.** `inv.craft(recipe, items)` bir `CallSpec<inventory::Craft>` kurar; `calls.call(..)`'a verilir. |
| `InventoryHandle` (Clone) | **Beklenebilir.** `inv_h.craft(recipe, items).await -> Outcome<inventory::Craft>`; `within(cx)` / `on_behalf_of(&id)` oyuncu kimliğini taşır. |
| `Inventory::serve(impl)` (trait'in sağlanmış metodu) | Hosta kaydedilen değer. |

Trait, makro tarafından `fn craft(&self, ..) -> impl Future<Output = ..> + Send`
biçimine çevrilir (RPITIT); impl'ler düz `async fn` yazar, kutu (Box)
yalnız yönlendiricide çağrı başına bir tane. Yanlış dönüş tipi impl'de
derleme hatasıdır (§17, `compile_fail`).

**Tower katmanları.** `|s| ..` kapanışı tower `ServiceBuilder`'ını saran
küçük bir `Setup`'tır: `.timeout(d)`, `.concurrency_limit(n)`,
`.load_shed()`, `.layer(l)`, `.mailbox(n)`. Katman hataları tipli sona
çevrilir: zaman aşımı → `CallError::TimedOut`, yük atma →
`CallError::Refused(Refusal::Shed)` (spike `live::tower_layers_end_calls_through_the_same_path`).
Servis aktörünün döngüsünde tek `await` (`recv`); her çağrı kendi
görevinde tek `await` (`oneshot` yığın üzerinden). Select yok, kilit yok.

### 4.2 İstemci-tel istekleri: `.proto`

```proto
import "gsb/options.proto";

service Game {
  option (gsb.reserved_op) = 1002;   // emekli op: bir daha kullanılmaz (§11)
  rpc Transfer(TransferGold)   returns (TransferDone) { option (gsb.op) = 1001; option (gsb.error) = "GameError"; }
  rpc Craft(CraftReq)          returns (Crafted)      { option (gsb.op) = 1003; option (gsb.error) = "GameError"; }
  rpc Friends(ListFriends)     returns (FriendList)   { option (gsb.op) = 1005; option (gsb.error) = "GameError"; }
}
```

(q3 düzeltmesi: rpc'nin adı istek mesajının adıyla **aynı olamaz** —
protoc `rpc TransferGold(TransferGold)`'un argümanını rpc'nin kendisine
çözer ve "is not a message type" der. İlk taslaktaki örnek bu yüzden
derlenmiyordu.)

Kod üretici (prost-build + küçük bir eklenti) her `rpc` için şunu yazar:

```rust
impl Call for TransferGold { type Response = TransferDone; type Error = GameError; }
impl Request for TransferGold { const OP: u16 = 1001; }
```

```rust
pub trait Call: Clone + Send + Sync + 'static {
    type Response: Clone + Send + Sync + 'static;
    type Error: Clone + Send + Sync + 'static;
}
pub trait Request: Call<Response: WireMessage, Error: WireMessage> + WireMessage {
    const OP: u16;
}
```

- Süreç içi çağrının (`inventory::Load`) cevabı düz bir Rust tipi
  olabilir (`InventoryData`, protobuf değil). İstemciye giden her şey
  `WireMessage`'tır (varsayılan: her `prost::Message`'a kapsayıcı impl).
- `Clone` sınırları: tablo, göçte yeniden yayın için çağrıyı ve girdi
  kaydı için sonucu kopyalar (§19 S5 — dosyaya kayıt serileştirme
  ister).
- Aynı `.proto`'dan C#/TS/GDScript istemcileri de üretilir (§14).

### 4.3 Kimlik

`Incoming<R>.from` ve `CallCx::player()` doğrulanmış bilet kimliğidir
(`ValidatedTicket.player`, registry'nin `ConnInfo.identity`'si). İstemci
yükünde "from" alanı olsa bile motor ona bakmaz. `.answers(req)` çağrıya
isteğin kimliğini koyar; isteksiz çağrı `.on_behalf_of(&identity)` ile
koyar; servisten servise `handle.within(cx)` taşır.

## 5. `Calls` API'si

### 5.1 Özet

| Yöntem | Ne yapar |
|---|---|
| `calls.take_incoming::<R>() -> Vec<Incoming<R>>` | Bu tick'in `R` istekleri, varış sırasıyla (tip başına FIFO). |
| `calls.take_incoming_all() -> Vec<AnyIncoming>` | Bu tick'in tüm istekleri, kanonik varış sırasıyla; `r.downcast::<R>()`. |
| `calls.reply(r.reply, Ok(..) / Err(..))` | İsteği şimdi cevapla (tipli). |
| `calls.call(spec)` → `.owner(o)` `.with(x)` `.answers(r.reply)` `.on_behalf_of(&id)` | Çağrı. Deyimin sonunda tabloya yazılır; FLUSH'ta gönderilir. |
| `calls.take_resolved::<C, X>() -> Vec<Resolved<C, X>>` | `C` türünden, `X` bağlamıyla verilmiş çağrıların sonuçları (`key`, `owner`, `ctx: X`, `outcome`). |
| `calls.in_flight::<C>(owner) -> bool` | Bu sahibin `C` çağrısı yolda (ya da bu tick yazıldı) mı? |
| `calls.forward(move \|r: R\| svc.method(r))` | Boot'ta, paylaşılan tek satırlık kural (§5.8). |
| `calls.export(owner) -> Exported` / `calls.import(exp, owner)` | Göç (§8). |
| `calls.accept::<R>()` | Boot'ta: oyun `R`'yi kendisi alır (op kaydı; çakışma `BootError`). |
| `calls.bind(conn, owner)` / `calls.unbind(conn)` / `calls.bound(conn)` | Join kancasında bir kez: oturum → sahip (§5.6). |

```rust
pub struct Incoming<R> { pub body: R, pub owner: Owner, pub from: Identity, pub conn: ConnId, pub reply: ReplyTo<R> }
pub struct Resolved<C: Call, X = ()> { pub key: CallKey, pub owner: Owner, pub ctx: X, pub outcome: Outcome<C> }
pub type Outcome<C> = Result<C::Response, CallError<C::Error>>;
pub enum CallError<E> { Service(E), Refused(Refusal), TimedOut, Abandoned }
pub enum Refusal { Full, Gone, Share, Shed }
```

`Incoming`'in alanları açıktır (`owner`: oturumun bağlı olduğu sahip,
§5.6): oyun `body`'nin parçalarını çağrıya
taşırken `reply`'ı `.answers(..)`'a verebilir (Rust kısmi taşıma). Bu
yüzden `.answers` isteğin tamamını değil `r.reply`'ı alır (onaylanan
`.answers(req)` biçiminden küçük bir sapma). `ReplyTo<R>` `Clone`
değildir: en fazla bir cevap.

### 5.2 Çağırmak ve kim sonucu alır

`calls.call(..)` bir oluşturucu döner; zincir bittiğinde (deyimin
sonunda, oluşturucu düşerken) çağrı tabloya yazılır — `.send()` yoktur.
Tüm metotlar isteğe bağlıdır. Sonucun gideceği yer kuraldır:

- `.owner(..)` ya da `.with(..)` verildiyse **ya da** `.answers(..)`
  verilmediyse → sonuç oyuna gelir (`take_resolved`).
- Yalnız `.answers(req)` verildiyse → sonuç **yalnız istemciye** gider;
  oyunun alacak bir şeyi yoktur (ve "alınmadı" diye sayılmaz).
- İkisi birden (`.owner(o).with(x).answers(req)`) → hem istemci
  cevaplanır hem oyun sonucu alır (üretimde kilitli malzemeleri çözmek
  için gerekli).

Owner verilmezse `Owner::ACTOR`'dür (aktörün kendisi; hep yaşar).

### 5.3 Ret, neden bir sonraki tick'te (KARAR)

`calls.call` asla beklemez ve asla `Err` dönmez. Dolu kutu (`Full`),
ölmüş servis (`Gone`), aktörün servisteki payı dolu (`Share`) — hepsi
**FLUSH'ta** (SYSTEMS'ten hemen sonra) kararlaşır ve **bir sonraki
tick'in RESPONSES'unda** `Err(CallError::Refused(..))` olarak, diğer
her sonla aynı `take_resolved` yolundan gelir. Gerekçe:

1. **Tek yol, tek zamanlama.** Oyun kodu "ret" için ayrı dal yazmaz;
   reddedilen çağrının rezervi, servis hatasıyla aynı kodla geri alınır.
   "Sonuç, onu veren tick'te asla görünmez" kuralı istisnasızdır.
2. **Deterministik ve kaydedilebilir.** Ret bir RESPONSES girdisidir;
   girdi kaydına diğer sonlarla aynı sırada girer, REPLAY'de kayıttan
   gelir (REPLAY'de servis yok — ret yerinde kararlaşsaydı tekrar
   oynatma onu üretemezdi).
3. **Tick içinde döngü yok.** Aynı tick'te teslim edilseydi "ret →
   yeniden dene" yapan bir sistem tick içinde dönebilirdi.
4. Bedel: ret bir tick geç görünür (≤ 33 ms @30 Hz) — dolu bir servise
   karşı önemsiz.

Alternatif "aynı tick'in temizliğinde" (BROADCAST'ta) teslim: oyun o
tick'te artık sistem koşmadığı için sonucu ancak bir sonraki tick görür
— pratikte aynı şey, ama kayıtta farklı bir yerde ve ikinci bir teslim
noktası. Elendi.

`.answers(req)` bağlı bir çağrı reddedilirse istemci motor cevabını alır
(`ok=false, game_error=false`, `reason = "service busy"` vb.; spike
`refusal::a_refused_answers_call_answers_the_client_with_the_reason`).

**Pay (share):** aktör başına, servis başına uçuştaki çağrı sınırı
(`RoomConfig.share`, varsayılan 64). Bir oda bir servisi tek başına
dolduramaz; s0'ın boyutlandırma kuralı (`N·k ≤ C`) aynen geçerli.

### 5.4 Tick sonunda kapatılanlar

| Durum | Motorun yaptığı | Sayaç |
|---|---|---|
| İstek hiç alınmadı (`take_incoming` çağrılmadı) | istemciye `"request not handled this tick"` | `requests_untaken` |
| İstek alındı ama cevaplanmadı / çağrıya bağlanmadı (`ReplyTo` düşürüldü ya da saklandı) | istemciye `"request taken but not answered"` | `requests_unanswered` |
| Kapatılmış isteğe sonradan `reply` | yok sayılır | `requests_replied_late` |
| Bağlanmamış oturumdan istek (READ'de) | istemciye `"session not bound to an owner"`; oyun görmez | `requests_unbound` |
| Sonuç geldi, kimse `take_resolved` etmedi | atılır | `responses_unhandled` |
| Sonuç geldi, sahibi artık yok | oyuna verilmez (istemci `.answers` ile yine cevaplanır) | `responses_owner_gone` |

"Sahibi yaşıyor mu" testini adaptör verir: bevy_ecs `world.get_entity`,
hecs `world.contains`, ECS'siz oyun kendi `alive(owner)`'ını. Sahip,
**sonucun değiştireceği durumun asıldığı şeydir**: ganimet örneğinde
(§6.3) sahip ganimet varlığıdır; ganimet süresi dolup yok olduysa
sonuç atılır (toplayan oyuncu yaşıyor olsa bile) — oyuncunun almasını
istiyorsanız sahip oyuncu olmalı.

### 5.5 Sahip ve ECS'ler: çekirdekte ECS yok, adaptörler isteğe bağlı

**Kural: çekirdeğin (`Calls`, sözleşme trait'leri, hostlar, oda) hiçbir
ECS bağımlılığı yoktur — isteğe bağlı bir özellik arkasında bile.**
`Calls` düz bir struct'tır. ECS entegrasyonu çekirdeğin DIŞINDAKİ ince,
isteğe bağlı adaptör crate'lerindedir; oyun kullandığını seçer, hiçbirini
kullanmayan oyun `&mut Calls` geçer. Söz konusu olan yalnız ECS
kütüphaneleridir (`bevy_ecs` crate'i, `hecs`, `shipyard`) — oyun sunucusu
bir oyun motoruna ihtiyaç duymaz, Bevy motoru hiçbir yerde yoktur
(çalışma alanı bugün de yalnız `bevy_ecs` crate'ini kullanıyor:
`gsb-ecs`, `gsb-kit`, demolar).

| Adaptör crate'i (motor adı önerisi) | İçeriği |
|---|---|
| `gsb-calls-bevy-ecs` (bağımlılık: yalnız `bevy_ecs`) | `GsbCalls(Calls)` yeni tipi: `Resource` + `Deref`/`DerefMut` (sistemler `ResMut<GsbCalls>` üzerinde `calls.call(..)` yazar, `&mut Calls` gibi); `Svc<T>` istemci kaynağı; `Tick` kaynağı; `owner.entity()` / `e.to_owner()`; tek iş parçacıklı yürütücüyle bir oda biçimi. |
| `gsb-calls-hecs` (bağımlılık: yalnız `hecs`) | Yalnız `owner.entity() -> Option<Entity>` / `e.to_owner()` (+ küçük bir oda biçimi). Tablo `&mut Calls` geçer. |
| (shipyard) | Aynı sahip modeli: `EntityId` ⇄ `u64` (`to_inner`/`from_inner`) — birkaç satırlık uzantı, ayrı kod gerekmez. |
| — (ECS yok) | Hiçbir adaptör: `Owner(oyunun_id'si)`, `&mut Calls`. |

```rust
// bevy_ecs (gsb-calls-bevy-ecs)
use gsb_calls_bevy_ecs::{EntityExt, GsbCalls, OwnerExt, Svc};
fn sys(mut calls: ResMut<GsbCalls>, inv: Res<Svc<InventoryClient>>, q: Query<..>) { .. }

// hecs (gsb-calls-hecs): sistemler düz fonksiyon
use gsb_calls_hecs::{EntityExt, OwnerExt};
fn sys(w: &mut hecs::World, calls: &mut Calls, inv: &InventoryClient) { .. }

// ECS'siz: owner = oyunun kendi id'si
calls.call(inv.load(who)).owner(Owner(player_id));
```

Neden yeni tip ve uzantı trait'i (`From` ya da `impl Resource for Calls`
değil): adaptör crate'inde `Calls` da `Resource` da `Entity` de yabancı
tiplerdir; yetim kuralı `impl Resource for Calls` ve
`impl From<Owner> for Entity`'yi yasaklar. Yeni tip ve yerel uzantı
trait'leri yasal ve özellik bayrağı istemez. (`to_owner`: `as_owner` adı
clippy'nin `wrong_self_convention` kuralına takıldı.)

bevy_ecs'te tablo kalıcı olarak dünyadadır (`GsbCalls` kaynağı); adaptör
motor fazlarında onu `world.resource_scope` ile ödünç alır (tablo +
"sahip yaşıyor mu" testi aynı anda gerekir). Şema tek iş parçacıklı
yürütücüyle koşar (determinizm; sistemlerin hepsi zaten
`ResMut<GsbCalls>` ister).

### 5.6 Oturum → sahip bağlama

Oyuncu odaya katıldığında oyun, join kancasında **bir kez** bağlar:

```rust
// bevy_ecs
Control::Join { conn, identity } => {
    let e = world.spawn((Player { conn: *conn, who: identity.clone() }, Wallet::default(), Bag::default(), JustJoined)).id();
    world.resource_mut::<GsbCalls>().bind(*conn, e.to_owner());
}
// hecs:   calls.bind(*conn, world.spawn((..)).to_owner());
// ECS'siz: calls.bind(*conn, Owner(id));
```

O andan sonra motor her `Incoming<R>`'ye `owner`'ı doldurur (`from` ve
`conn` da kalır); sistemler arama yazmaz: `q.get_mut(r.owner.entity())`.

- **Ayrılışta** motor CONTROL'de bağı kendisi çözer (`unbind`).
- **Göçte** `export(owner)` o sahibe bağlı oturumları da taşır, `import`
  onları yeni sahibe bağlar (spike `migration`).
- **Neden join kancasında `bind` (kancanın dönüş değeri değil):** her
  ECS biçiminde aynı tek satır; kanca bugün `Admission` döndürüyor ve
  sahip, kabul kararından sonra (varlık doğunca) belli olur. Motor
  entegrasyonunda `Admission::Accept` yanında isteğe bağlı bir sahip
  taşıyabilir; spike'ta `bind` yeterli. CONTROL READ'den önce koştuğu
  için katılımla aynı tick'te gelen istek zaten bağlıdır (spike `binding`).
- **Bağlanmamış oturumdan istek (KARAR):** motor
  `"session not bound to an owner"` cevaplar, `requests_unbound` sayar,
  oyun **hiç görmez**. Gerekçe: sahipsiz bir istekle oyunun yapabileceği
  bir şey yok — görseydi her sistem bir "sahipsiz" dalı yazardı (tam da
  kaldırmak istediğimiz tören). Bu durum yalnız bir hatada (oyun
  bağlamayı unuttu) ya da yarışta (ayrılış/göç sonrası gecikmiş istek)
  olur; ikisi de adıyla sayılmalı. İstisna: `forward` kuralları sahip
  istemez (meta istekler bağdan bağımsızdır) ve bağsız oturumda da
  çalışır.

### 5.7 Sıralama

1. **Tip başına FIFO:** `take_incoming::<R>()` isteklerini varış
   sırasıyla verir.
2. **Kanonik sıra:** `take_incoming_all()` tick'in tüm isteklerini READ'in
   çektiği sırayla verir (girdi kaydının sırası da budur).
3. **Tipler arası sıra = sistem sırası.** Ayrı `take_incoming` çağrıları
   kullanan oyun, farklı tipleri sistemlerinin sırasıyla görür (önce
   ganimet sistemi koşuyorsa önce bütün ganimet istekleri). Bu bilinçli
   bir takastır: sistemler kendi tiplerini alır, ortak bir dağıtıcı
   gerekmez. Kanonik sıra gereken oyun `take_incoming_all` kullanır
   (spike `ordering`: aynı 5 istek iki yoldan iki farklı, ikisi de
   tanımlı sırayla).
4. **Sonuçlar** varış sırasıyla gelir (her `(C, X)` kovası içinde); FLUSH
   retleri o tick'in kanal varışlarından önce. Farklı servisler arasında
   sıra yoktur.
5. **Çağrılar** servise FLUSH'ta yazıldıkları sırayla gider.

### 5.8 `forward`: paylaşılan tek satırlık kural

```rust
// boot'ta, her odanın kurulumunda (aynı satır)
calls.forward(move |r: ListFriends| friends.list(r))?;
```

Gelen `ListFriends` doğrudan arkadaş servisine gider, cevabı doğrudan
istemciye; oyunun sistemleri onu **hiç görmez** (spike `forward`:
`take_incoming_all` boş, `responses_delivered = 0`). Servis isteyenin
kimliğini `cx.player()`'da görür. Cevap/hata tipleri isteğinkilerle
aynı olmalı — değilse derleme hatası (§17). Bir op ya `accept` ya
`forward` edilir; ikisi birden boot hatasıdır. Kullanım: her odada
geçerli "meta" istekler (arkadaş listesi, bakiye, posta kutusu) — hub'a
gerek bırakmaz. (Onaylanan `forward::<R>(&svc.method)` biçimi Rust'ta
ifade değil; §19 S2.)

## 6. Örnekler

Ortak sözleşme (`.proto`'dan üretilmiş): `TransferGold → TransferDone`,
`PickUp → PickedUp`, `CraftReq → Crafted`, `Equip → Equipped`,
`ListFriends → FriendList`, hepsinin hatası `GameError { code }`.
Servisler: `Bank::transfer`, `Inventory::{load, grant, craft, save}`,
`Friends::list`. Oturumlar join kancasında bağlıdır (§5.6), bu yüzden
hiçbir sistem "oyuncuyu bağlantıdan bul" araması yazmaz. Tam kodu
spike'ta: `scenario/tests/common/{bevy_game,hecs_game,plain_game}.rs`
— üçü aynı senaryoda **bayt bayt aynı** istemci cevaplarını ve tick
başına aynı durum hash'lerini üretir (`three_ways`).

### 6.1 Arkadaşa altın gönderme (rezerv, tek ücret, `.answers`)

**bevy_ecs** (fonksiyon sistemi):

```rust
pub struct Reserve(pub u64);   // çağrının bağlamı: odada kalır, servise gitmez

fn transfers(
    mut calls: ResMut<GsbCalls>,
    bank: Res<Svc<BankClient>>,
    mut q: Query<(Entity, &Player, &mut Wallet, &Bag)>,
) {
    // 1) Önce sonuçlar: rezervi çöz, onaylanan bakiyeyi yaz.
    for r in calls.take_resolved::<bank::Transfer, Reserve>() {
        if let Ok((_, _, mut w, _)) = q.get_mut(r.owner.entity()) {
            w.reserved -= r.ctx.0;
            if let Ok(done) = &r.outcome { w.gold = done.balance; }
        }
    }
    // 2) Sonra yeni istekler.
    for r in calls.take_incoming::<TransferGold>() {
        let Ok((_, _, mut w, bag)) = q.get_mut(r.owner.entity()) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        if calls.in_flight::<bank::Transfer>(r.owner) {
            calls.reply(r.reply, Err(err(BUSY)));                  // çift tık: hemen
        } else if w.gold - w.reserved < r.body.amount {
            calls.reply(r.reply, Err(err(INSUFFICIENT)));
        } else {
            w.reserved += r.body.amount;                            // SENKRON rezerv
            calls.call(bank.transfer(Identity::new(&r.body.to), r.body.amount))
                .owner(r.owner)
                .with(Reserve(r.body.amount))
                .answers(r.reply);                                  // istemciye otomatik
        }
    }
}
```

Servis tarafı (`cx.player()` gönderendir, yükten değil; anahtarla tek
etki):

```rust
impl Bank for PgBank {
    async fn transfer(&self, cx: &CallCx, to: Identity, amount: u64) -> Result<TransferDone, GameError> {
        let from = cx.player().ok_or(err(NOT_LOADED))?;
        self.db.transfer_once(cx.key(), from, &to, amount).await   // aynı anahtar = aynı sonuç
    }
}
```

**hecs** — aynı mantık, sorgu biçimi farklı:

```rust
fn transfers(w: &mut hecs::World, calls: &mut Calls, bank: &BankClient) {
    for r in calls.take_resolved::<bank::Transfer, Reserve>() {
        let Some(e) = r.owner.entity() else { continue };
        if let Ok(wallet) = w.query_one_mut::<&mut Wallet>(e) {
            wallet.reserved -= r.ctx.0;
            if let Ok(done) = &r.outcome { wallet.gold = done.balance; }
        }
    }
    for r in calls.take_incoming::<TransferGold>() {
        let Some(Ok((wallet, bag))) = r.owner.entity().map(|e| w.query_one_mut::<(&mut Wallet, &Bag)>(e)) else {
            calls.reply(r.reply, Err(err(NOT_LOADED)));
            continue;
        };
        /* aynı üç dal; çağrı: .owner(r.owner).with(Reserve(..)).answers(r.reply) */
    }
}
```

**ECS'siz** — sahip oyunun kendi id'si: `self.players.get_mut(&r.owner.0)`;
gerisi aynı (spike `plain_game.rs`):

```rust
for r in calls.take_resolved::<bank::Transfer, Reserve>() {
    if let Some(p) = self.players.get_mut(&r.owner.0) {
        p.wallet.reserved -= r.ctx.0;
        if let Ok(done) = &r.outcome { p.wallet.gold = done.balance; }
    }
}
```

Zaman çizelgesi (spike `transfer`, bankanın test gecikmesi 3 tick):
t3'te iki tık (id 10, 11) → ilki rezerv + çağrı, ikincisi aynı tick'te
`BUSY`; t4'te id 10 yeniden → motor "duplicate request id" (oyun
görmez); t3–t5 arası cüzdan `(500, rezerv 100)`; t6'nın RESPONSES'unda
`.answers` istemciyi cevaplar (`balance 400`), sistem rezervi çözer →
`(400, 0)`. **Bankada tek ücret.**

### 6.2 Envanter 1: girişte yükleme

```rust
fn load_on_join(mut calls: ResMut<GsbCalls>, inv: Res<Svc<InventoryClient>>,
                joined: Query<(Entity, &Player), With<JustJoined>>, mut commands: Commands) {
    for (e, p) in &joined {
        calls.call(inv.load(p.who.clone())).owner(e.to_owner());
        commands.entity(e).remove::<JustJoined>();
    }
}

fn apply_loads(mut calls: ResMut<GsbCalls>, mut q: Query<(&mut Wallet, &mut Bag)>) {
    for r in calls.take_resolved::<inventory::Load, ()>() {
        let Ok((mut wallet, mut bag)) = q.get_mut(r.owner.entity()) else { continue };
        if let Ok(data) = r.outcome { wallet.gold = data.gold; bag.fill(&data.items); }
    }
}
```

Yükleme bitmeden gelen istekler `!bag.loaded → NOT_LOADED` ile cevaplanır;
"yükleniyor mu" sorusu `calls.in_flight::<inventory::Load>(e.to_owner())`.
hecs'te `load_on_join` önce `(Entity, Identity)` listesini toplar, sonra
çağırır ve `JustJoined`'ı `w.remove_one` ile kaldırır (sorgu ödüncü
bitmeden dünya değiştirilemez); ECS'siz oyunda `p.joined` bayrağı.

### 6.3 Envanter 2: ganimet toplama (bekleyen durum ganimet varlığında)

```rust
pub struct Picker(pub Owner);

fn loot(mut calls: ResMut<GsbCalls>, inv: Res<Svc<InventoryClient>>,
        mut bags: Query<(Entity, &Player, &mut Bag)>, mut piles: Query<(Entity, &mut Loot)>,
        mut commands: Commands) {
    for r in calls.take_resolved::<inventory::Grant, Picker>() {
        match r.outcome {
            Ok(got) => {
                commands.entity(r.owner.entity()).despawn();                    // ganimet gitti
                if let Ok((_, _, mut bag)) = bags.get_mut(r.ctx.0.entity()) {
                    bag.items.push(Item { kind: got.item, locked: false });
                }
            }
            Err(_) => if let Ok((_, mut l)) = piles.get_mut(r.owner.entity()) { l.claimed = false },
        }
    }
    for r in calls.take_incoming::<PickUp>() {
        let Ok((_, p, _)) = bags.get(r.owner.entity()) else { /* NOT_LOADED */ continue };
        let Some((le, mut l)) = piles.iter_mut().find(|(_, l)| l.id == r.body.loot) else { /* NO_SUCH_LOOT */ continue };
        if l.claimed { calls.reply(r.reply, Err(err(CLAIMED))); continue }      // kaybeden hemen duyar
        l.claimed = true;                                                       // bekleyen durum ganimette
        calls.call(inv.grant(p.who.clone(), l.item))
            .owner(le.to_owner())                                               // sahip = ganimet
            .with(Picker(r.owner))                                              // toplayan = isteğin sahibi
            .answers(r.reply);
    }
}
```

Spike `inventory`: aynı tick'te alice ve bob aynı ganimeti ister →
FIFO ile alice talep eder, bob aynı tick'te `CLAIMED` alır; ganimet
t4–t5 boyunca `claimed = true`, t6'da yok olur ve alice'in çantasına
girer. İkinci ganimet t6'da süresi dolup yok olur, bob'un ona ait
sonucu t7'de gelir → `responses_owner_gone = 1`, bob yine cevaplanır.

### 6.4 Envanter 3: kilitli malzemeyle üretim + `.answers`

```rust
pub struct Locked(pub Vec<ItemId>);

fn craft(mut calls: ResMut<GsbCalls>, inv: Res<Svc<InventoryClient>>, mut q: Query<(Entity, &Player, &mut Bag)>) {
    for r in calls.take_resolved::<inventory::Craft, Locked>() {
        let Ok((_, _, mut bag)) = q.get_mut(r.owner.entity()) else { continue };
        match r.outcome {
            Ok(made) => { bag.consume_locked(&r.ctx.0); bag.items.push(Item { kind: made.item, locked: false }); }
            Err(_) => bag.unlock(&r.ctx.0),                // servis hatası, ret, zaman aşımı: hepsi burada
        }
    }
    for r in calls.take_incoming::<CraftReq>() {
        let Ok((_, _, mut bag)) = q.get_mut(r.owner.entity()) else { continue };
        if !bag.try_lock(&r.body.consume) {
            calls.reply(r.reply, Err(err(MISSING)));
        } else {
            calls.call(inv.craft(r.body.recipe, r.body.consume.clone()))
                .owner(r.owner)
                .with(Locked(r.body.consume))              // kısmi taşıma: body.consume buraya,
                .answers(r.reply);                         // reply buraya
        }
    }
}
```

Malzemeler çağrı süresince kilitlidir (başka bir üretim/kuşanma onları
göremez); sonuç her durumda kilidi çözer. İstemciye `Crafted` ya da
`GameError` otomatik gider; servis kimliği `cx.player()`'dan alır.

### 6.5 Envanter 4: kuşanma (servissiz) + otomatik kayıt

```rust
fn equip(mut calls: ResMut<GsbCalls>, mut q: Query<&mut Bag>) {
    for r in calls.take_incoming::<Equip>() {
        let Ok(mut bag) = q.get_mut(r.owner.entity()) else { continue };
        if bag.has_unlocked(r.body.item) {
            bag.equipped = Some(r.body.item);
            bag.dirty = true;
            calls.reply(r.reply, Ok(Equipped { item: r.body.item }));   // aynı tick
        } else {
            calls.reply(r.reply, Err(err(MISSING)));
        }
    }
}

fn autosave(mut calls: ResMut<GsbCalls>, inv: Res<Svc<InventoryClient>>, tick: Res<Tick>,
            mut q: Query<(Entity, &Player, &mut Bag)>) {
    for r in calls.take_resolved::<inventory::Save, ()>() {
        if r.outcome.is_err() && let Ok((_, _, mut b)) = q.get_mut(r.owner.entity()) { b.dirty = true }
    }
    if tick.0.tick() % 5 != 4 { return }
    for (e, p, mut bag) in &mut q {
        if bag.dirty && !calls.in_flight::<inventory::Save>(e.to_owner()) {     // oyuncu başına tek kayıt
            calls.call(inv.save(p.who.clone(), bag.kinds(), bag.equipped.unwrap_or(0))).owner(e.to_owner());
            bag.dirty = false;
        }
    }
}
```

Kayıt sonucu da alınmalıdır (başarılıysa yok sayılır) — alınmazsa
`responses_unhandled` sayar. Bu bilinçli: "kimse bakmadı" bir kayıptır.

## 7. Servisten servise ve admin: `Handle`

```rust
struct GiftShop { bank: BankHandle }

impl Shop for GiftShop {
    async fn gift(&self, cx: &CallCx, to: Identity, price: u64) -> Result<TransferDone, GameError> {
        self.bank.within(cx)                 // aynı oyuncu adına
            .transfer(to, price).await       // tek await; Outcome<bank::Transfer>
            .map_err(|e| match e { CallError::Service(g) => g, _ => err(UNAVAILABLE) })
    }
}

// admin / boot kodu
let r = bank_handle.on_behalf_of(&Identity::new("bob")).transfer(carol, 25).await;
```

Handle çağrısı kutu dolu/servis yok için hemen `Err(Refused(..))` döner
(beklenebilir yolda "hemen" anlamlıdır), cevap kanalı düşerse
`Abandoned`. Handle çağrıları hiçbir odanın kaydında değildir (oda dışı
çağıran — determinizm sınırının dışında). Spike
`live::a_service_calls_another_through_its_handle`.

**Oda dışı aktör** (zamanlayıcı, turnuva yöneticisi): kendi `Calls`
tablosu + kendi tick'i olan bir aktör — aynı API, oda olması gerekmez.

## 8. Göç: `export` / `import`

```rust
// kaynak oda, varlık seam'i geçerken (ya da oda başka shard'a taşınırken)
let pending = calls.export(player.to_owner());          // anahtarlar + bağlamlar + .answers hedefleri
mig.push(pending);                                       // oyunun göç yükünde taşınır

// hedef oda
calls.import(pending, new_entity.to_owner());            // FLUSH'ta AYNI anahtarla yeniden yayın
```

- Kaynak girişleri bırakır (`calls_exported`); kaynağa sonradan gelen
  cevap `results_late` sayılır, hiçbir yere uygulanmaz.
- Hedef aynı `CallKey` ile yeniden gönderir; **idempotent servis**
  anahtarla tekilleştirir → tek etki. Hedef sonucu bağlamıyla alır;
  `.answers` hedefi de taşındığı için istemci hedeften cevaplanır (s0
  S7'nin "göçte istemci isteği bırakılır" duruşu böylece düzelir).
- O sahibe bağlı oturumlar da taşınır: kaynakta bağ çözülür, `import`
  onları yeni sahibe bağlar (§5.6).
- Spike `migration`: A çağırdı, banka ücretlendirdi; A export, B import;
  B aynı anahtarla yeniden yayınladı → **tek ücret**, B `(Owner(9), 100,
  Ok(400))` aldı ve istemciyi cevapladı, A'nın cevabı `results_late`.
- s0'ın reçetesiyle aynı sözleşme: anahtarı saklamayan servis göç
  altında en-az-bir-kez semantiğine razıdır. Yeniden başlatmada (C1
  checkpoint) oyun anahtarları checkpoint'e yazar.
- Bu spike'ta `Exported` süreç içidir (bağlam `Box<dyn Any>`); süreçler
  arası göç bağlamın serileştirilmesini ister (§19 S8).
- **Motorda (q5, B152):** shard göçü bunu kendisi yapar —
  `PlayerMigration.calls`; ayrıntı §20 "Faz 5 — uygulanan biçim".

## 9. Her kaybı say: sayaçlar

Her biri tek anlamlı (B32). Spike `Counters`; motorda metriklere bağlama
uygulama turunda.

**Çağrı (gönderen tarafı):** `calls_issued`, `calls_sent`,
`calls_refused_full`, `calls_refused_gone`, `calls_refused_share`,
`calls_exported`, `calls_imported`.
**Sonlar:** `results_ok`, `results_service_error`, `results_refused`
(FLUSH retleri + servisteki `Shed`), `results_timed_out`,
`results_abandoned`, `results_late`.
**Teslim:** `responses_delivered`, `responses_unhandled`,
`responses_owner_gone`, `answers_sent`.
**İstekler:** `requests_received`, `requests_malformed`,
`requests_no_handler`, `requests_undecodable`, `requests_duplicate`,
`requests_unbound`,
`requests_forwarded`, `requests_answered`, `requests_untaken`,
`requests_unanswered`, `requests_replied_late`.
**Tekrar:** `replay_diverged`.
**Servis tarafı** (`ServiceStats`, atomik): `calls_received`,
`replies_undeliverable_full`, `replies_undeliverable_gone`.

**Defter kimlikleri:**
`calls_issued = calls_sent + calls_refused_{full,gone,share}`;
`calls_issued = results_ok + results_service_error + results_refused +
results_timed_out + results_abandoned + calls_exported + yolda`
(spike senaryosunda `calls_issued == results_ok`); bir sonucun teslimi:
`results_* (geç olmayan) = responses_delivered + responses_owner_gone +
yalnız-.answers`.

## 10. Determinizm

### 10.1 Ne kaydedilir (oda sınırında, opt-in)

Her tick için, kanonik sırayla: `dt`; CONTROL girdileri; RESPONSES'ta
teslim edilen her sonuç (`CallKey`, çağrı tipi, sonucun kopyası — geç
gelenler yalnız anahtar); READ'in çektiği istekler (`ConnId`,
`RpcRequest`); (EVENTS, sonraki tur). Başlıkta oda id'si ve **tohum**.
Kayıt her tick'in durum hash'ini de tutar.

Determinizmi sağlayan diğer parçalar:

- `ctx.time()` / `ctx.dt()`: oda saati = kaydedilen `dt`'lerin toplamı.
  Mantıkta duvar saati yok.
- `ctx.rng()`: oda başına, tohumlu (SplitMix64); tohum kayıtta.
- **Deterministik çağrı anahtarı:** `CallKey { origin, seq }`, `origin =
  mix(tohum ^ mix(oda id))`, `seq` FLUSH sırası. Tekrar aynı anahtarları
  üretir; REPLAY kayıtlı sonuçları anahtarla eşler.
- Tick başına **durum hash kancası:** oyun doldurur
  (`fn state_hash(&mut self, h: &mut StateHasher)`); motor rng durumunu ve
  oda saatini ekler. Hile doğrulama = istemcinin/denetçinin tekrarındaki
  hash'leri canlı hash'lerle karşılaştırmak.

### 10.2 REPLAY modu

`Room::replay(log, ..)`: aynı oyun (aynı ikili), `ReplayHost`'tan
alınmış istemcilerle kurulur (rotaları hiçbir yere gitmez). Tablo
REPLAY'de **hiç göndermez**; FLUSH yalnız bekleyen girişi yazar;
RESPONSES kanal yerine kayıttaki listeyi teslim eder (anahtarla eşleyip,
girişin tipiyle kopyalayarak). CONTROL ve READ kayıttan. Spike
`determinism::a_live_run_replays_to_the_same_hashes_without_calling_services`:

- CANLI koşu: tokio üzerinde 0–8 ms rastgele gecikmeli servisler, tick
  arası gerçek uyku, ölçülen `dt`, kayıt açık;
- iki kez REPLAY: tick başına hash'ler canlı koşununkiyle **aynı**,
  istemci cevapları aynı tick'lerde aynı, servisin çağrı sayacı
  **değişmedi**;
- negatif kontrol: kayıttaki ilk sonucu bir tick geciktir → hash'ler
  ayrışır (hash zamanlamayı görüyor).

**Spike bulgusu — ayrışma algılama:** negatif kontrolün ilk hâlinde oyun
farklı çağrılar üretti; aynı `seq` artık başka tipte bir çağrıya aitti
ve kayıttaki sonuç yanlış tipe kopyalanmaya çalışıldı (panik). Çözüm:
kayıt her sonucun **çağrı tipini** de tutar; REPLAY'de tip uyuşmazsa
sonuç uygulanmaz, `replay_diverged` sayılır. Gerçek bir tekrarda (aynı
ikili, deterministik oyun) hiç olmaz; olursa "bu kayıt bu oyunla
oynatılamaz" demektir.

### 10.3 TEST modu

```rust
let mut sim = Sim::new();
sim.service(Delay::ticks(3, Bank::serve(FakeBank::default())), 64);   // 3 tick sonra görünür
sim.service(Delay::never(Inventory::serve(FakeInv::default())), 64);  // hiç cevaplamaz
let mut room = Room::new(cfg, build(Clients::from(&sim)));
for _ in 0..10 { room.tick_sim(&mut sim); }                          // sabit dt, tohumlu rng
```

Sahte servis **sadece bir trait impl'idir**; motor yalnız zamanlamayı
kontrol eder: tick sonrası `sim.step` posta kutularını çeker, servis
future'larını no-op waker ile yoklar, cevabı `Delay`'in dediği tick'te
bırakır (`Delay::ticks(n, ..)`: çağrının tick'inden `n` tick sonraki
RESPONSES). Çalışma zamanı yok, tower katmanları yok (zamanlama
motorun). `sim.kill::<inventory::Kind>()` "servis öldü"yü canlandırır
(yeni çağrılar `Gone`, tutulanlar `Abandoned`). Spike: aynı tohumla iki
koşu aynı (hash + cevaplar), farklı tohum farklı. (`Delay` impl'i değil,
`serve(..)` değerini sarar: `Delay::ticks(3, Bank::serve(fake))`.)

### 10.4 Oyunun kaçınması gerekenler

- `HashMap`/`HashSet` **iterasyon sırasına** bağımlılık (arama sorun
  değil; sıralı iterasyon için `BTreeMap` ya da ECS sorgusu).
- Mantıkta duvar saati (`Instant::now`, `SystemTime`) ve kendi RNG'si.
- bevy_ecs'te çok iş parçacıklı şema yürütücüsünün sırasına güvenmek
  (adaptör tek iş parçacıklı koşar).
- Tekrarı farklı bir ikiliyle oynatmak (`replay_diverged` yakalar,
  ama düzeltemez).

Oda dışı çağıranlar (Handle ile çağıran servisler, admin) kaydedilmez;
odaya etkileri yalnız sonuç olarak girer ve o zaten kayıttadır.

## 11. Sürümleme ve gözlemlenebilirlik

- **Op'lar asla yeniden kullanılmaz.** `accept`/`forward` op'u boot'ta
  talep eder; ikinci talep `BootError::OpCollision { op, first, second }`
  (spike `boot`). Kaldırılan bir rpc'nin op'u `.proto`'da `reserved`
  olarak kalır (kod üretici denetler).
- **Bilinmeyen op** → motor `"no request handler for op"`; çözülemeyen
  yük → `"undecodable request payload"`; id 0 → `"malformed"`. Hiçbiri
  oyuna ulaşmaz.
- **İzleme:** `CallKey::trace_id()` anahtardan türetilir (yeniden
  yayında aynı → göç sonrası çağrı aynı izde kalır). Servis katmanı her
  metot için `service=Inventory method=craft trace=…` span'i + gecikme
  histogramı yayar (bir tower katmanı; OTLP ihracatçısı mevcut). Spike
  span yaymaz; `ServiceStats` sayaçlarını tutar.

## 12. Oyunun işi

Yetkilendirme (bu oyuncu bu ganimeti alabilir mi), istek başına hız
sınırı (bir istemci saniyede kaç `Craft` yollayabilir), sayfalama
(arkadaş listesinin 2. sayfası) — oyun sistemlerinde ya da servisin
içinde. Motor yalnız bağlantı başına bekleyen istek sınırını ve
servis payını uygular.

## 13. Neyi kaldırdık, neyi taşıdık

- `RequestDecision::External(future)` → motorun yerleşik bir `forward`
  kuralı olur (future'ı koşan yerleşik servis + `.answers`); tel aynı,
  sayaç anlamları korunur. Geçiş: tipli tabloda bulunmayan op bugünkü
  `handle_request`'e düşer.
- s0'ın `TickCtx<'a, D>`, `GameLogic::{Done, Req, on_done, on_request}`,
  etiket kapanışları, `Refused::fail`, `RequestTable<In>`,
  `SystemCtx::ext` önerileri — hepsi gereksiz: `Calls` tablosu oyunun
  her yerinden (bevy_ecs'te `ResMut<GsbCalls>`, diğerlerinde `&mut Calls`)
  erişilir.
- Hub ve onun tel değişiklikleri (`RpcRequest.target`, `RPC_RESP`) —
  §18 A5.

## 14. İstemci ve FFI

İstemci tarafı s0'daki gibi (sans-IO Rust çekirdeği, kare başına
`poll`, sabit C ABI), şu farklarla: hub hedefi yok; istek id'sini
**sarmalayıcı** atar; iptal `Command::Forget { id }`; `.proto`'dan
üretilmiş tipli istemci.

```proto
// ffi.proto (istemci çekirdeği ↔ sarmalayıcı; telde değil) — s0'dan farklar
message Command {
  oneof kind {
    JoinRoom join = 1; LeaveRoom leave = 2; SendAction action = 3;
    Request request = 4;      // { uint64 id; uint32 op; bytes payload }  (target YOK)
    Disconnect disconnect = 5;
    Forget forget = 6;        // { uint64 id } — sarmalayıcı vazgeçti; geç cevap çekirdekte düşer (sayılır)
  }
}
enum Status { STATUS_OK = 0; STATUS_GAME_ERROR = 1; STATUS_ENGINE_REJECTED = 2;
              STATUS_LOCAL_TIMEOUT = 3; STATUS_SESSION_ENDED = 4; }
```

Rust tarafı (tur q3): FFI çekirdeğinin istek yarısı
`gsb_client::requests::Requests`'tir (sans-IO; `forget` = `Forget`,
`RequestError`'ın ilk beş kolu = `Status`'ün beş değeri); bugün
`RequestClient` onu doğrudan bir `Conn` üstünde koşar.

C ABI beş fonksiyon (s0 ile aynı): `gsb_abi_version`, `gsb_new`,
`gsb_command`, `gsb_poll(buf, cap) -> len` (`< 0` → `-gereken`; olay
çekirdekte bekler), `gsb_free`. `unsafe` yalnız `gsb-ffi`'de.

**C# (Unity) — üretilmiş istemciyle:**

```csharp
// .proto'dan üretilir (protoc eklentisi): op'lar ve tipler tek yerde
public sealed partial class GameClient {
    readonly GsbClient gsb;
    public GameClient(GsbClient gsb) => this.gsb = gsb;
    public Task<TransferDone> TransferGold(TransferGold req, CancellationToken ct = default)
        => gsb.Request<TransferGold, TransferDone, GameError>(1001, req, ct);
    public Task<Crafted> Craft(CraftReq req, CancellationToken ct = default)
        => gsb.Request<CraftReq, Crafted, GameError>(1003, req, ct);
}

// el yazımı çekirdek sarmalayıcı (bir kez)
public Task<TRes> Request<TReq, TRes, TErr>(uint op, TReq req, CancellationToken ct)
    where TReq : IMessage<TReq> where TRes : IMessage<TRes>, new() where TErr : IMessage<TErr>, new() {
    var id = ++nextId;                                     // id'yi sarmalayıcı atar
    var tcs = new TaskCompletionSource<TRes>(TaskCreationOptions.RunContinuationsAsynchronously);
    pending[id] = r => {
        switch (r.Status) {
            case Status.Ok:        tcs.TrySetResult(new MessageParser<TRes>(() => new TRes()).ParseFrom(r.Payload)); break;
            case Status.GameError: tcs.TrySetException(new GameException<TErr>(new MessageParser<TErr>(() => new TErr()).ParseFrom(r.Payload))); break;
            default:               tcs.TrySetException(new GsbException(r.Status, r.Reason)); break;
        }
    };
    ct.Register(() => { if (pending.Remove(id)) { Send(new Command { Forget = new Forget { Id = id } }); tcs.TrySetCanceled(); } });
    Send(new Command { Request = new FfiRequest { Id = id, Op = op, Payload = req.ToByteString() } });
    return tcs.Task;
}

// oyun kodu
try {
    var done = await game.TransferGold(new TransferGold { To = "bob", Amount = 100 });
    ui.Gold = done.Balance;
} catch (GameException<GameError> e) when (e.Error.Code == Codes.Busy) { /* çift tık */ }
```

`Poll()` s0'daki gibi (`MonoBehaviour.Update`'ten; `n < 0` → tamponu
`-n`'e büyüt, aynı olayı yeniden çek; `Status.GameError` = sunucunun
`game_error` bayrağı). Godot (gdext) ve JS (wasm) aynı `Command`/`Event`
protosunu Rust çekirdeği üzerinden konuşur; üretilmiş GDScript/TS
istemcileri aynı `.proto`'dan.

## 15. OLAYLAR (sistemin bize ittiği) — sonraki tur, yeniden tasarlanacak

> **Bu bölüm s0 metnidir ve bu turda DEĞİŞTİRİLMEDİ** (yalnız hub'a
> düşüş işaretlendi). s0b'nin kararlarıyla çelişen yerleri var (hub'a
> düşüş, `on_events` kancası); olaylar bir sonraki turda `Calls`
> tablosuyla aynı biçimde (`take_events::<E>()` benzeri) yeniden
> tasarlanacak. Tick'teki yeri (READ ile CONVERT arası) korunuyor.

### 15.1 API (s0)

```rust
#[derive(Clone)]
pub struct Pusher { /* yönlendiricinin kutusu */ }   // servislere kompozisyonda verilir
pub enum Dest { Client, World }

impl Pusher {
    pub fn push<E: Event>(&self, identity: &str, dest: Dest, e: &E) -> Result<(), PushRefused>; // try_send
    pub fn push_many<E: Event>(&self, identities: &[&str], dest: Dest, e: &E) -> Result<(), PushRefused>; // yük bir kez kodlanır
}
// Oda tarafı:
fn on_events(&mut self, w, ctx, events: &mut Vec<PushIn>) {
    for e in events.drain(..) { if let Some(b) = e.decode::<BalanceChanged>() { … } }
}
```

Kimlik = doğrulanmış bilet kimliği (`ValidatedTicket.player`, TICKETS),
registry'nin zaten tuttuğu `ConnInfo.identity`.

### 15.2 Yönlendirme: iki hedef, tek yol (s0)

```text
servis ──Pusher.push──▶ [registry: kimlik → canlı oturum]
                              │ yok → pushes_offline / ayrık (park) → pushes_world_parked
                              ▼
                     bağlantı aktörü (ConnIn::Push)
             Dest::Client │                    │ Dest::World
                          ▼                    ▼
              PUSH karesi (kendi yolu)   üyelik kanalı (Inbound::Push) ──▶ READ ayırır ──▶ EVENTS
              odanın haberi olmaz        oda yok → (s0: hub'a; s0b'de hub yok) pushes_world_no_room
```

**Neden `Dest::World` oturumun üyelik kanalından gider (oda başına
ayrı bir itme kutusu yerine):** üyelik kanalı oturumu **yapısal olarak**
izler — resume'da yarılar takas edilir, shard göçünde kanal oturumla
birlikte taşınır. Oda başına kutu + "kimlik → oda" yönlendirmesi
sharded odada çöker: registry oyuncunun hangi shard'da olduğunu bilmez
(leave'i bugün tüm shard'lara yayınlıyor); yayın + "sahibi alır"
yapılırsa geçiş anındaki itme hiçbir shard'a ait olmaz ve **sayılamaz**
(N shard'ın her biri "bende değil" der). Oturum kanalı ise tam bir
sayaç verir: dolu (`pushes_world_dropped_full`), kapalı
(`pushes_world_dropped_closed`), bağsız oturum (`pushes_world_unbound`,
`requests_dropped_unbound`'ın ikizi). Bedel: itme, istemcinin girdi
kanalının kapasitesini paylaşır — kendi kanalını dolduran bir istemci
kendi itmelerini düşürür (yalnız kendine zarar, sayılır). READ'de itme
işareti istemcinin pull bütçesine sayılmaz.

**Park edilmiş oyuncu** (oturumu yok, varlığı dünyada): ilk sürümde
`Dest::World` itmesi `pushes_world_parked` olarak sayılır, teslim
edilmez — otorite servistedir, resume'da oyun yeniden eşitler.

**Çevrimdışı** teslim (kutu, davet saklama) motorun işi değil: servis
saklar, oyuncu giriş yapınca oyun kendi isteğiyle çeker. Motor yalnız
`pushes_offline` sayar.

**Yetenek bayrağı.** Eski bir istemci `PUSH` opcode'unu bilmez; ona
asla gönderilmez: `Auth.features` (yeni alan) `PUSH` bitini taşımayan
bağlantıya `Dest::Client` itmesi `pushes_unsupported` olarak sayılır.

**Neden yönlendirici registry (ilk sürüm):** kimlik→oturum tablosu
zaten orada (auth/close olaylarıyla güncel). Risk: sohbet hacmi kontrol
düzleminin kutusuna biner (join/leave gecikmesi). Ölç-sonra-böl.

## 16. Tel değişiklikleri (eklemeli — DESIGN §5)

Bu turun istek tarafı için tek ek:

```proto
message RpcResponse {
  uint64 id = 1; bool ok = 2; uint32 op = 3; string reason = 4; bytes payload = 5;
  // ok=false iken: true → payload oyunun tipli hatası (Request::Error);
  // false → motor reddi, metin `reason`'da (bugünkü tek şekil).
  bool game_error = 6;
}
```

Neden bayrak (s0 bulgusu): varsayılan değerli bir proto3 hata mesajı
sıfır bayta kodlanır; "yük boşsa motor reddi" kuralı onu yanlış okur.
`PROTOCOL_VERSION` artmaz; eski istemci `game_error`'ı okumaz, `ok=false`
görür. s0'ın `RpcRequest.target`/`RPC_RESP` önerileri hub'la birlikte
düştü; `Push`/`PUSH`/`Auth.features` olaylar turunda.

Oyun tarafı `.proto` için `gsb/options.proto`:

```proto
extend google.protobuf.MethodOptions {
  uint32 op = 50001;     // (gsb.op)
  string error = 50002;  // (gsb.error): hata mesajının adı
}
```

## 17. Spike: `spikes/s0-calls`

gsb çalışma alanının üyesi değil (kök `exclude`), kendi küçük çalışma
alanı. Motora dokunmaz; kendi senkron tick'ini modeller. Yapısı motorun
kuralını aynalar — **çekirdekte ECS yok**:

| Crate | İçerik | ECS bağımlılığı |
|---|---|---|
| `s0-calls` (kök) | sözleşme, `Calls`, hostlar (canlı/test/tekrar), oda, ECS'siz biçim | **yok** (`cargo tree` ile doğrulandı) |
| `macros/` (`s0-calls-macros`) | `#[service]` | yok |
| `adapters/bevy-ecs` (`s0-calls-bevy-ecs`) | `GsbCalls`, `Svc`, `Tick`, uzantılar, `BevyRoom` | yalnız `bevy_ecs` crate'i |
| `adapters/hecs` (`s0-calls-hecs`) | uzantılar, `HecsRoom` | yalnız `hecs` |
| `scenario/` (`s0-calls-scenario`) | tüm testler (çekirdek dev-bağımlılık olarak bile ECS çekmesin diye ayrı) | ikisi de |

Çalıştırma: `cd spikes/s0-calls && cargo test --workspace` (23 test + 1
doc testi + 5 `compile_fail`; her crate'in `build.rs`'i `gsb-lint`'i
koşar). Bağımlılıklar: tokio (sync/rt/time), tower 0.5, prost; adaptörlerde
bevy_ecs 0.19 / hecs 0.10 (hecs yalnız spike); syn/quote.

| Gereksinim | Test |
|---|---|
| Arkadaşa altın uçtan uca: rezerv, tek ücret, `.answers` ile cevap; çift tık + aynı id → tek ücret | `transfer` (2) |
| Dört envanter deseni (girişte yükleme, ganimette bekleyen durum, kilitli üretim + `.answers`, servissiz kuşanma + otomatik kayıt) | `inventory` |
| Ret (dolu / yok / pay) `take_resolved`'dan, bir sonraki tick; `.answers`'lı retin istemci cevabı; `Shed` | `refusal` (4) + `live` |
| Alınmayan sonuç, cevaplanmayan/alınmayan istek, geç cevap kapatılır + sayılır; sahip yok sayılır | `refusal` (3) |
| `forward`: oyun görmez, istemci cevaplanır, kimlik servise geçer; forward+accept çakışması | `forward` (2) |
| Tip başına FIFO ve `take_incoming_all` kanonik sırası | `ordering` |
| Göç: export → import → aynı anahtar → tek ücret; kaynağın geç cevabı | `migration` |
| Servisten servise `Handle`, admin'den doğrudan `Handle`; tower timeout + load_shed | `live` (2, gerçek tokio) |
| Aynı senaryo bevy_ecs / hecs / ECS'siz → aynı cevaplar, aynı tick hash'leri, aynı servis etkisi | `three_ways` |
| Rastgele gecikmeli canlı koşu kaydı → iki REPLAY aynı hash'ler, servis çağrılmadı; negatif kontrol; TEST modu iki koşu aynı | `determinism` (2) |
| Yanlış cevap tipi derleme hatası (5 biçim: `.answers`, `reply`, servis impl'i, `take_resolved`, `forward`) | `src/lib.rs` doc testleri |
| Boşta maliyet: tick başına iki boş `try_recv`, tampon kapasitesi 0 | `idle_cost` |
| Op çakışması boot'ta; bilinmeyen/bozuk/id 0 motor cevabı | `boot` (2) |
| Oturum bağlama: `Incoming.owner` doldurulur; bağsız oturum → motor cevabı + `requests_unbound`, oyun görmez; ayrılış bağı çözer; göç bağı taşır | `binding`, `migration` |

**Bulgular:**

1. **Kayıt fonksiyonu yetim kuralına takılır** (§19 S1): oyun crate'inde
   `impl<T: Inventory> gsb::ServiceImpl<..> for T` yazılamaz (yabancı
   trait, örtük tip parametresi önde). Spike `Inventory::serve(impl)`
   biçimini kullanır; makro ayrıca kendi yönlendirici tipini oyun
   crate'inde üretir (motorun `Router<S>`'i için `tower::Service` impl'i
   de aynı kurala takılırdı).
2. **`&svc.method` Rust ifadesi değildir** (alıcıya bağlı metot değeri
   yok) → `forward` kapanış alır (§19 S2).
3. **bevy_ecs `Resource`** adaptör crate'inden `Calls`'a verilemez (yetim
   kuralı) → adaptörde `GsbCalls` yeni tipi; çekirdek ECS'siz kalır
   (§5.5). Sistemler `ResMut<GsbCalls>` üzerinde `Deref` ile aynı kodu
   yazar; üç biçimin testi değişmeden aynı sonucu verdi.
4. **REPLAY ayrışması** (§10.2): kayıt çağrı tipini tutmalı;
   `replay_diverged`.
5. `RoomLogic::state_hash` `&mut self` ister: bevy_ecs sorguları dünyaya
   `&mut` ister (sorgu durumu kurulumu).
6. Tek iş parçacıklı bevy_ecs yürütücüsü determinizm için şart; adaptör
   ayarlar.
7. `async fn` → RPITIT (`impl Future + Send`) dönüşümü 1.95'te temiz
   çalışıyor; impl'ler düz `async fn` yazıyor, `async-trait` gerekmiyor.
8. Canlı koşuda senaryonun kaç çağrı yaptığı bile gecikmeye bağlı
   (yükleme t3'ten sonra gelirse transfer `NOT_LOADED` olur) — kaydın
   neden şart olduğunun canlı örneği.
9. Doc testlerinde `use super::*` üreten makro fonksiyon gövdesinde
   çalışmaz (gövde içi öğeler alt modülden görünmez) → bloklar
   `fn main() {}` içerir; gerçek kodda (modül düzeyinde) sorun yok.
   Kararlı rustdoc `compile_fail`'de hata kodunu denetlemez; beşi elle de
   derlendi ve beklenen kodla düştü (E0271, E0308, E0271, E0308, E0271).
10. Kilitsiz sahte servis: `Mutex` yasak olduğundan sahte arka uç
    atomikler + CAS'lı sabit boyutlu bir anahtar→sonuç tablosu
    (idempotency deposu) kullanır — gerçek servisler bunu veritabanında
    yapar.

**Kanıtlayamadıkları:** gerçek oda/shard aktörü ve registry; gerçek tel
baytları (`wire.rs` ayna); `.proto` kod üretici (spike `impl Call/Request`'i
elle yazar, üreticinin çıktısı gibi); span/OTLP; dosyaya kayıt ve onun
biçimi; ayırma sayımı (sayan `GlobalAlloc` `unsafe` ister — yapısal
kanıt); süreçler arası göç; FFI ve dil sarmalayıcıları; yük altında
maliyet.

## 18. Elenen alternatifler

| # | Alternatif | Neden elendi |
|---|---|---|
| A1 | `Done` enum'u + etiket kapanışları (s0) | Her oyun bir "bitti" enum'u ve eşleme yazar; kapanış serileştirilemez (göç/kayıt); ECS sistemleri `TickCtx`'e ulaşamaz. `take_resolved::<C, X>()` aynı tip güvenliğini enum'suz verir. |
| A2 | Devamlar (continuation: `call(..).then(\|world, r\| ..)`) | Kapanış dünyayı yakalar → ödünç alma çatışması, serileştirilemez, tekrar oynatılamaz; mantık zamanla dağılır. |
| A3 | Bileşende yoklanan `Pending<T>` handle'ları | Her tick O(handle) yoklama, toplu teslim yok, kayıp sayımı dağınık; varlık silinince handle sessizce kaybolur. |
| A4 | Bileşenleri depolama olarak kullanmak (sonuç = eklenen bileşen) | ECS'e bağımlı (hecs/ECS'siz oyun?), aynı varlığa iki sonuç çakışır, alınmayan sonuç sayılamaz, motor her ECS için ayrı yazılır. |
| A5 | Hub (s0: üyesiz, simülasyonsuz oda + bağlantının kendi cevap yolu) | İkinci oda türü + ikinci cevap yolu (`RPC_RESP`, `target`); "her oyuncu her zaman bir odada" ile gereksiz: lobi bir odadır, maç içindeki meta istekler `forward` ile her odada çözülür. |
| A6 | Ret `call()`'dan hemen `Err` olarak | İki hata yolu (hemen + sonra); REPLAY'de servis yokken üretilemez; sistem tick içinde yeniden deneyebilir. §5.3. |
| A7 | Tick içinde `await`/`block_on` | Tek-await kuralı; senkron tick'in maliyet garantileri gider. |
| A8 | Tipi silinmiş sonuç + oyunun çalışma zamanında downcast'i | Silme yalnız motorun içinde; oyun her yerde tipli (`Resolved<C, X>`). |
| A9 | Göçte cevabı yeni odaya yönlendir | Zincir göç, yeniden başlatmada imkânsız; anahtarla yeniden yayın üçünü birden çözer (s0 §11). |
| A10 | Bağlamı servise gönder, servis geri yollasın | Zaman aşımı/ölümde bağlam kaybolur, rezerv kilitli kalır; servis oyun tipini bilir. |
| A11 | `async-trait` crate'i | Her çağrıda kutulanmış future + `Send` belirsizliği; RPITIT 1.75'ten beri kararlı. |
| A12 | İstek tipi ile süreç içi çağrı tipini tek trait'te birleştirmek | Süreç içi tipler protobuf olmak zorunda kalırdı; `Call`/`Request` ayrımı `InventoryData` gibi düz tipleri serbest bırakır. |
| A13 | `Calls`'ı ayrı bir aktör yapmak (kullananlar kanalla erişir) — kullanıcı kararı 2026-10-07: tablo | Tick senkron: `in_flight`/`take_*` bir kanal turu bekleyemez, cevap bir tick sonra gelirdi; çift harcama koruması aynı tick'te senkron cevap ister (tablo yine yerelde tutulurdu → iki kopya); odanın sınırına ikinci bir zamanlama belirsizliği girerdi (determinizm); her çağrıya fazladan aktör atlaması; paylaşılacak durum yok (her çağıranın kendi tablosu), tek aktör tüm odaların darboğazı olurdu. Kanallar zaten aktörlerin arasında: servis posta kutusu ve tamamlanma kanalı. Tablo, bağlantı tablosu gibi aktörün kendi durumudur. |

## 19. Açık sorular (her biri öneriyle)

> **Takip (s0b-2):** S1 sapması (`serve(..)`), S2, S4, S5 önerileri
> **onaylandı**; S3 ve S14 aşağıdaki biçimde **kapandı**.

1. **S1 — Kayıt biçimi** (ONAYLANDI: (a)) (onaylanan `server.service(PgInventory::new(db), ..)`
   yetim kuralı yüzünden olduğu gibi yazılamıyor). Seçenekler:
   (a) `server.service(Inventory::serve(PgInventory::new(db)), ..)` —
   spike'ta çalışan; bir tip birden çok servis trait'i uygulasa bile
   belirsiz değil; (b) impl bloğuna da öznitelik:
   `#[gsb::service] impl Inventory for PgInventory {..}` → makro somut
   `impl ServiceImpl<inventory::Kind> for PgInventory` üretir, onaylanan
   satır **aynen** çalışır (tip birden çok servis uygularsa turbofish
   gerekir); (c) turbofish: `server.service::<inventory::Kind, _>(x, ..)`.
   **Öneri: (a)**; tek öznitelik, tek kural. (b) "aynı satır" için
   istenirse eklenebilir, (a) ile birlikte yaşar.
2. **S2 — `forward` biçimi** (ONAYLANDI: kapanış) (`&svc.method` Rust'ta değer değil).
   Spike: `calls.forward(move |r: ListFriends| friends.list(r))` — `R`
   kapanış parametresinden çıkar, tipler derlemede denetlenir. Seçenek:
   makro, tek argümanı bir `Request` olan metotlar için
   `friends.forward_list()` üretebilir → `calls.forward(friends.forward_list())`.
   **Öneri:** kapanış; şeker gerekirse sonra.
3. **S3 — bevy_ecs `Resource`** — KAPANDI. Kullanıcı kararı: çekirdek
   hiçbir ECS'e bağımlı değildir (özellik bayrağı arkasında bile).
   bevy_ecs entegrasyonu isteğe bağlı `gsb-calls-bevy-ecs` crate'indeki
   `GsbCalls(Calls)` yeni tipidir (`Resource` + `Deref/DerefMut`) ve
   uzantılardır; hecs/shipyard yalnız sahip uzantısı alır (§5.5). Bevy
   oyun motoru hiçbir yerde yoktur — yalnız `bevy_ecs` crate'i.
4. **S4 — Tick'i aşan cevap** (ONAYLANDI) (eşleştirme kuyruğu gibi "birkaç tick sonra
   cevaplarım" istekleri). Şimdi: tick sonunda `requests_unanswered`.
   **Öneri:** açık bir `calls.hold(r.reply, ticks) -> Held<R>` (bileşende
   saklanabilir, süre sonunda motor cevaplar ve
   `requests_hold_expired` sayar) — ilk gerçek ihtiyaçta.
5. **S5 — Girdi kaydının dosya biçimi** (ONAYLANDI). Süreç içi sonuçlar (`InventoryData`)
   serileştirme ister; spike yalnız bellekte tutar (`Clone`). **Öneri:**
   `record` özelliği açıkken `Call` tiplerine `WireMessage` (ya da serde)
   sınırı; makro bu sınırı özellikle ekler. Biçim: oda başına dosya,
   tick başına uzunluk önekli çerçeve, başlıkta ikili sürümü + tohum.
6. **S6 — Oda tarafı süre süpürmesi** (servis hiç cevaplamaz ve timeout
   katmanı yoksa giriş sonsuza kadar yolda; spike'ta süpürme yok).
   **Öneri:** `server.service` varsayılan bir timeout katmanı koysun (ör.
   30 s, kapatılabilir) + RESPONSES'ta yalnız yolda çağrı varken koşan
   tick tabanlı güvenlik süpürmesi (`results_timed_out`; kayda RESPONSES
   girdisi olarak girer → tekrar oynatılabilir).
7. **S7 — Kapasite varsayılanları.** Pay 64/servis, tamamlanma kanalı
   1024. **Öneri:** tamamlanma kapasitesi ≥ 2 × Σpay, yapılandırma
   doğrulamasıyla (s0'ın kuralı).
8. **S8 — Süreçler arası göç.** `Exported` bugün süreç içi (`Box<dyn Any>`
   bağlam). **Öneri:** `distributed` özelliğinde `.with(x)` için
   `X: Serialize` sınırı; DISTRIBUTED turuna.
9. **S9 — Yanlış `X` ile `take_resolved`.** Sessizce boş döner, tick
   sonunda `responses_unhandled`. **Öneri:** debug derlemede tick
   sonunda bekleyen `(C, X)` tip adlarını günlüğe yaz.
10. **S10 — İz yayılımı.** `within(cx)` bugün yalnız oyuncuyu taşır.
    **Öneri:** `CallCx` bir üst iz id'si taşısın; `within` onu ayarlasın;
    span'ler bağlansın.
11. **S11 — `.proto` kod üretici nerede?** **Öneri:** `gsb-build` crate'i
    (prost-build + `(gsb.op)`/`(gsb.error)` okuyan küçük üretici);
    `reserved` op denetimi orada.
12. **S12 — "Sonucu kim alır" kuralı** (§5.2: owner/with yoksa ve
    `.answers` varsa yalnız istemci). Alternatif: her zaman oyuna da ver
    (`.answers`'lı çağrılar için `responses_unhandled` gürültüsü).
    **Öneri:** spike'taki kural.
13. **S13 — FFI'da `unsafe`** (s0 S11). **Öneri:** değişmedi — tek, küçük,
    denetlenen `gsb-ffi` istisnası.
14. **S14 — "Oyuncuyu bağlantıdan bul" tekrarı** — KAPANDI: oturum →
    sahip bağlama (`calls.bind` join kancasında, `Incoming.owner`,
    bağsız oturum → `requests_unbound`; §5.6). Örneklerdeki taramalar
    kalktı.

## 20. Uygulama planı

**Faz 1 — çekirdek `Calls` (s1a; tel değişikliği yok)**
- `gsb-core`: `contract` (`Call`, `Request`, `WireMessage`, `CallKey`,
  `CallCx`, `Outcome`), `Calls` tablosu, `CallSpec`/`Route`/`ReplySink`,
  sayaçlar → metrikler.
- Oda ve shard aktörlerinde fazlar: `phase_completions` → RESPONSES;
  READ'de istek çözümü tabloya (oturum → sahip bağı, `requests_unbound`);
  FLUSH; tick sonu kapatma; ayrılışta `unbind`. `GameLogic`'e
  tablo erişimi ve `alive(owner)` (spike'taki `RoomLogic::with_calls`
  deseni, §5.5).
- `External` → yerleşik forward; `handle_request` geçiş yolu.
- Kapılar: hedefli testler ajanda, tam set ebeveynde.

**Faz 1 — uygulanan biçim (tur q1).** Tasarımdan farklar ve
somutlaşanlar:

- **Tablo aktörün durumudur** (A13): oda/shard `Calls`'ı kendisi tutar,
  oyuna `GameLogic::update_with_calls(world, ctx, &mut Calls)` ile ödünç
  verir (varsayılanı `update`; mevcut oyunlar değişmeden derlenir).
  Boot kaydı `GameLogic::boot_calls`; çakışma `BootError` olarak
  günlüğe yazılır (aktörün kurulumu hatasızdır; ilk kayıt kalır).
  Shard'da `ShardLogic::update_seam_with_calls` (varsayılanı
  `update_seam`).
- **Bağlama:** motor her oturumu varsayılan olarak oyuncusuna bağlar
  (`GameLogic::call_owner` → `Owner::from(PlayerId)`, devam ve göçte
  sabit); `None` oturumu bağsız bırakır (`requests_unbound`).
  `calls.bind` açık bağdır, oturum bitince (ayrılış/park/göç/üstüne
  katılım — CONTROL) çözülür. "Sahip yaşıyor mu":
  `GameLogic::owner_alive` (varsayılan: aktörün tuttuğu oyuncu).
  `Owner` artık `gsb_core::owner` (kit ile ortak; `from_bits/to_bits`).
- **`Incoming`** ayrıca `player: PlayerId` taşır.
- **Tel değişmedi:** tipli servis hatası `ok=false`, BOŞ `reason`,
  `payload` = hata (motor cevaplarında `reason` hep dolu); `game_error`
  bayrağı Faz 3'te.
- **`External`** tablonun yerleşik çağrısıdır: tek bekleyen tablosu, tek
  tamamlanma kanalı, son tarih RESPONSES'ta tick saatiyle; eski sayaçlar
  (`requests_external/timed_out/late/abandoned`, caps) aynen. Eski
  `rpc::PendingRequest`/`rpc::Completion` kalktı.
- **Sayaçlar:** `requests_malformed/duplicate/no_handler` için mevcut
  `requests_rejected_*` sayaçları iki yola da hizmet eder; oturumu biten
  isteklerin cevapları `requests_abandoned`/`requests_undelivered`.
  `requests_received`, `replay_diverged` yok (Faz 4).
- **Servis tarafı:** `Route<M>` (sınırlı posta kutusu), `Mail<M>`,
  `Envelope<M>`, tipli `Ask<C>` (`answer`/`fail`; düşerse `Abandoned`),
  `CallSpec::new(&route, call, Variant)`; `calls::serve(name, cap,
  handler) -> (Route, Service)` (F5 durdurma). q2'nin makrosu/tower hostu
  aynı dikişe oturur.
- Ertelenenler BACKLOG'da `q1:` satırları.

**Faz 2 — servis tarafı (s1b)**
- `gsb-macros`: `#[gsb::service]` (spike makrosu, motor yolu `::gsb`).
- Canlı host: `RegistryParts::service(Inventory::serve(..), |s| ..)`
  (F5 `Service` durdurma sırasıyla birleşik), tower bağımlılığı,
  varsayılan timeout (S6), span + gecikme katmanı (OTLP).
- Demo ekonomisinin `#[gsb::service] trait Economy`'ye geçişi.

**Faz 2 — uygulanan biçim (tur q2).** Tasarımdan farklar ve
somutlaşanlar:

- **Motor yolu `::gsb_core`, öznitelik `#[gsb_core::service]`.** Çalışma
  alanında `gsb` adlı şemsiye crate yok; makro (`crates/gsb-macros`)
  üretilen kodda `::gsb_core::calls::…` ve `::gsb_core::__private::tower`
  yazar, `gsb_core` `service`'i yeniden dışa açar (ve kendi testleri için
  `extern crate self as gsb_core`). Motora başka yoldan ulaşan bir crate
  `#[gsb_core::service(crate = benim::yolum)]` der (serde'nin `crate =`
  deyimi). Bir oyun `use gsb_core as gsb;` ile `#[gsb::service]` de
  yazabilir — gsb_core doğrudan bağımlılıksa.
- **İki mesaj: `Msg` ve `Bare`.** q1'in dikişinde (`CallSpec::new(&route,
  call, wrap)`) posta kutusu mesajının kolları `Ask<C>` taşır; tower
  katmanı (timeout, load_shed) çağrının future'ını düşürünce `Ask`'ın
  düşmesi `Abandoned` olurdu. Bu yüzden host, mesajı katmanlara girmeden
  `ServiceKind::split` ile ikiye ayırır: çıplak çağrı (`inventory::Bare`,
  tower'ın gördüğü `Dispatch<Bare>`) ve cevap hakkı (`Reply`, hostta
  kalır). Katman hatası hakkı tipli sonla kapatır: timeout →
  `TimedOut`, shed → `Refused(Shed)`, başka bir katman hatası →
  `Abandoned`. Elle yazılan servis (`calls::serve`) aynı `Msg`/`Ask`
  biçimini kullanmaya devam eder.
- **Kayıt.** `RegistryParts::service(X::serve(impl), |s| ..)` tower hostunu
  kurar ve `Service`'ini F5 listesine (kayıt sırasıyla) ekler; istemciler
  `parts.client::<XClient>()` / `parts.client::<XHandle>()`
  (`try_client` `Option` döner). Eski el yazımı kayıt
  `RegistryParts::service(Service)` → **`add_service(Service)`** oldu
  (iki argümanlı `service` ile aynı adı taşıyamazdı). Sunucusuz kullanım:
  `gsb_core::calls::Services` (aynı API, `into_stops`) ya da tek servis
  için `calls::spawn(served, setup) -> (Route, Service)`. Aynı tür iki
  kez kaydedilirse panik (kompozisyon kökü hatası) — q5'ten beri
  `Err(RegisterError::Duplicate)` (B169, Faz 5 notu).
- **`Setup`:** `.timeout(d)` (tower; varsayılan süreyi değiştirir),
  `.concurrency_limit(n)`, `.load_shed()`, `.layer(l)`, `.mailbox(n)`
  (varsayılan 1024), `.no_timeout()`. **S6:** her servisin varsayılan
  süresi `DEFAULT_SERVICE_TIMEOUT` = 30 sn (hostun kendi
  `tokio::time::timeout`'u, `.timeout(d)` yoksa); **tablonun kendi son
  tarihi** `CallsConfig::timeout` / `RoomConfig::call_timeout` (varsayılan
  60 sn = 2 × servis süresi, `None` kapalı): FLUSH'ta gönderilen çağrı
  RESPONSES'ta tick saatiyle süpürülür → `TimedOut`, mevcut
  `results_timed_out`; servisin geç cevabı `results_late`. Yeni oda
  sayacı yok; loadgen teli değişmedi.
- **Durdurma:** `Stop` mesajı kuyruktakilerin arkasına girer; host onları
  sunar, posta kutusunu kapatır (sonraki çağrı `Refused(Gone)`) ve
  başlattığı her çağrı cevaplanınca biter (düşme bariyeri). Sunucuda
  odalardan sonra (F5).
- **`Handle`:** `Route::ask` — dolu/yok kutu hemen `Refused(Full/Gone)`,
  tek await cevabın oneshot'ı; `CallCx.caller.actor = ActorId::HANDLE`.
  **S10 kapandı:** `CallCx` bir üst iz taşır; `within(cx)` hem oyuncuyu
  hem izi (`cx.trace_id()`) aktarır, `on_behalf_of(&id)` yeni iz.
  `CallCx::new(key, caller)` servis uygulamasını doğrudan test etmek için.
- **Gözlemlenebilirlik:** her çağrı `gsb.call` span'inde (`service`,
  `method`, `trace`); `ServiceStats`'a `calls_timed_out`, `calls_shed`,
  `calls_failed`, `calls_abandoned` (serviste panik) ve metot başına
  `MethodStats` (`calls`, `latency_us_total`, `latency_us_max`) eklendi.
  Dışa açılmıyorlar (BACKLOG `q2:`); OTLP ihracatçısı yalnız metrik
  taşıdığı için span'ler `tracing` olarak kalır.
- **Makro sertleştirmesi:** async olmayan, gövdeli, generic'li, `&self`
  ya da `cx: &CallCx` parametresi eksik, ödünç argümanlı (`&str`), `impl
  Trait` argümanlı, `Result` döndürmeyen metot, supertrait/generic'li
  trait, metotsuz servis ve üretilen adlarla çakışan metot (`serve`,
  `within`, `on_behalf_of`, …; çağrı tipi `Msg`/`Bare`/`Kind`/`Router`)
  derleme hatasıdır. `compile_fail` doc-testleri
  `gsb-core/src/calls/macro_checks.rs`'de (yanlış dönüş tipi, servis
  olmayan tipin `serve`'ü, bilinmeyen metot, yanlış argüman tipi, Clone
  olmayan argüman dahil).
- **Kit (B151):** `Game::{boot_calls, systems_with_calls, call_owner(world,
  player, entity), owner_alive}` + `ShardGame::systems_seam_with_calls`;
  her kit odası `GameLogic`/`ShardLogic` kancalarını iletir (ince
  iletim — kit'in ECS'ten bağımsızlaştırılması KIT-ECS'te ayrı iş).
- **`BootError` (B153, B159):** registry, tablosu kurulamayan odayı
  kurmaz — `CreateRoom` `CoreError::RoomBoot` (tel kodu `RoomOpFailed`),
  `/rooms/open` 500; panik sonrası yeniden kurulumda aynı hata odayı
  emekli eder. Yapıcılar hatasız kaldı (`boot_error()` erişimcisi).
  Motor aralığında (`< 1000`) op isteyen el yazımı istek
  `BootError::EngineOp`.
- **Config (B155):** `call_share`, `call_completions`, `call_timeout_secs`
  (yalnız düz anahtar); başlatma `call_completions ≥ 2 × call_share`'i
  denetler (tek servisli alt sınır; OPS §2).
- **Demo ekonomisi:** `#[gsb_core::service] trait Economy { buy }` +
  `PriceList`; sunucu modülü `parts.service(Economy::serve(..), |s|
  s.mailbox(64))` ile barındırır. **Odalar `External` yolunda kaldı:**
  `.answers` yolu reddi `game_error + DemoError` olarak taşır, demo telinin
  kilidi (`ok=false`, `reason` metni) değişirdi; oda future'ı
  `EconomyHandle`'ı bekler ve her sonu eski gerekçe metnine çevirir
  (`economy service gone/busy/timed out/dropped the reply`). `Buy`
  (`BuyItem`, 1006) `DemoRequests`'te bildirildi (B160).
- Ertelenenler BACKLOG'da `q2:` satırları.

**Faz 3 — `.proto` istekleri (s1c)**
- `gsb/options.proto`, `gsb-build` üretici (S11), `RpcResponse.game_error`
  (+ `wire_contract.rs` kilidi), op çakışma/`reserved` denetimi.

**Faz 3 — uygulanan biçim (tur q3).** Tasarımdan farklar ve
somutlaşanlar:

- **Sözleşme `gsb-protocol`'de:** `Call`, `Request`, `WireMessage`,
  `DecodeError` artık `gsb_protocol::request`'te; `gsb_core::calls`
  aynı trait'leri yeniden dışa verir (yollar değişmedi). Neden: tipli
  istemci (`gsb-client`) `R::OP`/`R::Response`/`R::Error`'a sunucu
  motorunu bağlamadan erişmeli.
- **`gsb/options.proto`** `gsb-protocol/proto/gsb/` altında (aynı
  `links` dizini, `DEP_GSB_BASE_PROTO_DIR`): `(gsb.op)` (50001, rpc),
  `(gsb.error)` (50002, rpc; aynı paketten ad ya da `.` ile tam ad),
  `(gsb.reserved_op)` (50003, servis, tekrarlanabilir — op uzayının
  `reserved`'ı). gsb-protocol'ün build'i dosyayı derleyerek denetler; Rust
  kodu üretilmez.
- **`gsb-build`** (yeni crate; prost-build üstünde saf Rust, protoc eklenti
  ikilisi yok, yeni sistem gereksinimi yok — vendored protoc): protoc'un
  tanımlayıcı kümesini kırpılmış `descriptor.proto` aynalarıyla okur
  (prost-types uzantıları düşürüyor), denetler, kodu prost-build'in
  `ServiceGenerator` kancasıyla isteğin kendi paket modülüne yazar:
  `impl Call` + `impl Request` (`OP`, `Response`, `Error`). Reddedilen
  build (hepsi listelenir): yinelenen op, motor aralığında op
  (`< GAME_BAND_START` = 1000), 16 biti aşan op, `reserved_op`'u alan rpc,
  `(gsb.error)`'suz ya da hiçbir mesajı adlandırmayan rpc, akışlı rpc, iki
  rpc'nin arkasında tek istek mesajı (tipin tek `OP`'u olur). Hiçbir rpc'si
  gsb seçeneği taşımayan servis sıradan protobuf servisidir, dokunulmaz;
  gsb servisinde her rpc'nin `(gsb.op)`'u olmalı.
- **Demo:** `game.proto`'daki `DemoRequests` `AbilityUse`'u (1005)
  `DemoError` ile bildirir; gsb-demo'nun build'i `gsb_build::Builder`'a
  geçti. Tel aynı (mevcut kilitler değişmeden geçti). `BuyItem` (1006)
  ekonomi göçüyle (q2) gelecek.
- **Tel:** `RpcResponse.game_error = 6` (eklemeli, §16). Tipli hata:
  `ok=false`, `game_error=true`, BOŞ `reason`, yük = hata (sıfır bayt
  olabilir). Motor cevabı: `game_error=false`, `reason` dolu. q1'in boş
  `reason` biçimi korundu: eski istemci aynı baytları görür (+ `30 01`).
  `PROTOCOL_VERSION` artmadı (DESIGN §5.5 kuralı: yeni alan eklemelidir,
  eski okuyucu yanlış AYRIŞTIRMAZ). Kilit:
  `wire_contract.rs::rpc_response_game_error_is_field_six_and_additive`.
  Eski `External`/`Reject` yolu değişmedi (`game_error=false`).
- **Tipli Rust istemcisi** (`gsb_client::requests`): sans-IO `Requests`
  (id'yi istemci verir — 1'den; `start`/`start_within` → zarf karesi,
  `answer`, `sweep(now)`, `end_session`, `forget(id)`, `take::<R>`) ve
  onu bir `Conn` üstünde koşan `RequestClient`
  (`client.request::<R>(&body).await`, `send`/`send_within` + `wait`).
  Sonuç `Result<R::Response, RequestError<R::Error>>`:
  `Game(E)` / `Engine(reason)` / `TimedOut` (istemcinin kendi saati, istek
  başına) / `SessionEnded` (S16: oturum bitince uçuştaki her istek kapanır)
  / `Undecodable` / `Forgotten`. Cevaplar oyunun seçtiği karede gelir
  (demo/kit: `Private.responses`); istemci bir çıkarıcı
  (`FnMut(&FrameBody) -> Option<Vec<RpcResponse>>`) alır; cevap olmayan
  kareler sınırlı bir kuyrukta bekler (taşan en eski düşer, sayılır).
  `wait` turda tek sınırlı `recv` (select yok). Sayaçlar istemci tarafı
  (`RequestCounters`: `sent`, `ok`, `game_errors`, `engine_errors`,
  `timed_out`, `session_ended`, `forgotten`, `answers_forgotten`,
  `answers_late`, `undecodable`) — metrik tablolarına/loadgen teline
  girmez; sunucuda yeni sayaç yok.
- Ertelenenler BACKLOG'da `q3:` satırları.

**Faz 4 — determinizm (s1d)**
- `TickCtx::{time, dt, rng}`, deterministik anahtar kökeni, durum hash
  kancası, `record` özelliği (S5), REPLAY modu + `replay_diverged`,
  `gsb-kit::testing`: `Sim` + `Delay`; loadgen/golden'e kayıt-tekrar
  senaryosu.

**Faz 4 — uygulanan biçim (tur q4).** Tasarımdan farklar ve
somutlaşanlar:

- **Modül:** `gsb_core::determinism` (ECS'siz; `Sim`/`SimRoom` çekirdekte,
  kit'te değil — kit testleri bevy'ye bağlı, sürücü her `W` ve her
  `RoomLogic` için).
- **Zaman ve rastgelelik:** `TickCtx` iki alan aldı: `time: Duration`
  (oda saati, bu adımın `dt`'si dahil) ve `rng: Rng<'_>` (odanın
  `RoomRng`'sinin ödünç görünümü — `Cell`, çünkü kancalar `&TickCtx`
  alır; kick kuyruğu deseni). `ctx.rng.next_u64() / below(n) / unit() /
  chance(p)`. Elle kurulan bağlamın varsayılanı ETKİSİZ tutamaçtır: iş
  parçacığına yerel, 0 tohumlu akış (test başına deterministik, kayda
  girmez). Shard da tohum/saat/rng alır (tohum `mix(seed ^ indeks)`); kayıt
  yalnız odada.
- **Tohum:** `RoomConfig::seed: Option<u64>`; `None` = enkarnasyon başına
  taze tohum (`determinism::fresh_seed`: std'nin OS tohumlu hasher anahtarı
  + süreç sayacı + oda id'si; yeni bağımlılık yok) — canlı anahtarlar
  yeniden doğuşlar arasında da benzersiz. Anahtar kökeni
  `mix(seed ^ mix(oda))` (q1'in `mix(oda)`'sının yerine).
- **Durum hash'i:** `GameLogic::state_hash(&mut self, &mut W) ->
  Option<u64>` (varsayılan `None`; `&mut W` bevy sorguları için). Motor
  sonucu rng durumu ve oda saatiyle katlar (`StateHasher` = FNV-1a 64,
  `std::hash::Hasher`). Yalnız kayıt, tekrar ve TEST modunda, adımın
  SONUNDA (yayından sonra) sorulur; kayıtsız canlı oda hiç sormaz.
- **Kayıt (`record` özelliği, kapalıyken kodu yok):**
  `RoomActor::with_record(Recorder)` / `SimRoom::record`. Kayıt oda
  sınırında değil **mantık sınırında** tutulur: oyunun gördüğü her girdi,
  odanın işlediği sırayla — CONTROL kancaları (join/resume/leave/
  disconnect/may_release/expire/batch dropped-resumed) ve oturum sonu,
  RESPONSES (her son: anahtar, çağrı tipi adı, sahip yaşıyor muydu,
  sonucun kopyası; External son tarihleri; geç gelenler yalnız anahtar),
  READ (tablonun aldığı istekler, `ingest`'in aksiyonları,
  `handle_request`'in gördüğü istekler), faz işaretleri (SYSTEMS, tick
  sonu) + `dt` + hash. Tıkanma/bozuk zarf gibi taşıma kararları mantığa
  ulaşmadığından kayıtta yoktur. Girdiler tabloda (`Calls`) tutulur; adım
  sonunda bir `TickRecord` sınırlı kanala `try_send` edilir (dolu/kapalı →
  `records_dropped`). `Input` `#[non_exhaustive]`: EVENTS için yuva.
- **Dosya (S5):** `write_log` / `spawn_writer` (kanalı `blocking_recv` ile
  boşaltan düz iş parçacığı — tick ve çalışma zamanı dosyaya dokunmaz) /
  `read_log`. Biçim: `GSBINLOG` + biçim sürümü + başlık (motor ve oyun
  sürümü, oda, tohum, düğmeler) + tick başına uzunluk önekli çerçeve;
  elle yazılmış LE, yeni bağımlılık yok. **Sapma (S5'in sınırı):** süreç
  içi çağrı tiplerine `record` altında `WireMessage` sınırı EKLENMEDİ —
  özellikler toplamsaldır; bir crate `record`'u açınca başka bir crate'in
  kodlanamayan çağrı tipleri derlenmez olurdu. Bunun yerine sonuç
  kodekleri tip adıyla kayıtlıdır: `forward` kuralının çağrısı kendiliğinden,
  diğerleri boot'ta `calls.record_as::<C>()` ile. Bellekten tekrar kodek
  istemez (`Clone` yeter); kodeksiz bir sonuç dosyaya "eksik" yazılır
  (yazar `outcomes_unencodable` sayar), tekrarda `replay_undecodable`.
- **REPLAY:** `Replay::new(Arc<InputLog>, world, logic)` → `tick()` /
  `run() -> ReplayReport { hashes, first_divergent, counts }`. Aktör yok,
  taşıma yok, çalışma zamanı yok: girdiler kayıttaki sırayla aynı kancalara
  verilir; tablo REPLAY'de hiç göndermez (çağrı anahtarıyla bekler,
  External geleceği koşturulmaz, süre süpürmeleri koşmaz), sonlar kayıttan
  anahtarla gelir ve yalnız **aynı tipteki** bekleyen çağrıya uygulanır
  (`replay_diverged` aksi hâlde — anahtarda bekleyen yoksa da);
  "sahip yaşıyor mu" kayıttaki cevaptır. İlk farklı hash'in tick'i
  raporlanır (hile doğrulama). Boşta-girdi ve yol görünümleri tekrarda
  BOŞ okunur (taşıma ölçümleri, kaydedilmez).
- **TEST modu:** `Sim::service(name, cap, Delay::ticks(n, handler))` —
  işleyici q1'in `serve` biçimi (`Fn(Envelope<M>) -> Future`); q2'nin
  üretilmiş servisi için `Sim::served(cap, Delay::ticks(n, X::serve(fake)))`
  (yönlendiricisi tower'sız koşar). T tick'inin FLUSH'ında verilen çağrı
  T+n'in RESPONSES'unda görünür; `Delay::never` tutar, `sim.kill(id)`
  `Abandoned` verir, sonraki çağrılar `Gone`. `SimRoom`: gerçek oda
  aktörü, sabit `dt` (periyot, tam sayı nanosaniye), tohumlu (verilmezse 0),
  her adımda hash, betikli istemciler (`SimClient::{request, raw_request,
  action, frames}`); `tick_at(Instant)` canlı zamanlamalı koşu için.
- **Testler** (`determinism/tests`): TEST modu aynı tohumla iki kez aynı
  (hash + cevaplar), farklı tohum farklı; `Delay::ticks(n)` tam n. tick;
  `Delay::never` + `kill`; üretilmiş servis TEST modunda; boştaki kayıtsız
  oda hiçbir tampon ayırmaz; anahtar kökeni tohumdan. `record` ile: tokio
  üzerinde 0–8 ms rastgele gecikmeli canlı koşu (External isteği ve
  aksiyonlar dahil, sahibi giden bir çağrı) kaydedilir → iki tekrar aynı
  hash'ler, aynı cevaplar, aynı son sayaçları, servis çağrılmadı, tablo hiç
  göndermedi; negatif kontrol (ilk RESPONSES bir tick geç) tam o tick'te
  ayrışır; tipi değiştirilmiş son `replay_diverged` sayılır, uygulanmaz;
  dosyaya yazılan kayıt bellektekiyle aynı oynar, kodeksiz yazılan kayıp
  iki tarafta sayılır; yazar iş parçacığı odanın kaydını yazar.
- **Sayaçlar:** `CallCounts`'a `replay_diverged`, `replay_undecodable`,
  `records_written`, `records_dropped` (satır, Prometheus, OTLP, golden'ler;
  loadgen teli `CALL_COUNT` dizisiyle 4 alan büyüdü — sihir sayısı
  `GSNT` değişmedi, bir sonraki boş `GSNU`).
- Ertelenenler BACKLOG'da `q4:` satırları.

**Faz 5 — ECS adaptörleri ve kit**
- Çekirdek ECS'siz kalır; isteğe bağlı adaptör crate'leri
  `gsb-calls-bevy-ecs` (yalnız `bevy_ecs`: `GsbCalls`, uzantılar, oda
  biçimi) ve `gsb-calls-hecs`; kit dikişleri. (shipyard istenirse aynı
  birkaç satır.)

**Faz 5 — uygulanan biçim (tur q5).** Tasarımdan farklar ve
somutlaşanlar:

- **Crate adları: ECS başına tek bağlayıcı** (KIT-ECS K10 önerisi (b)):
  `gsb-bevy-ecs` ve `gsb-hecs`, her biri bir `calls` modülüyle (§5.5'teki
  `gsb-calls-bevy-ecs`/`gsb-calls-hecs` adları yerine). Gerekçe: oyun
  ECS'ini bir kez seçer; kit'in bağlayıcısı (KIT-ECS §5.9'un
  `gsb-kit-bevy-ecs`/`gsb-kit-hecs`'i) aynı crate'e `kit` modülü olarak
  katılır, crate sayısı yarıya iner; `gsb-ecs`'in `System`/`SystemRunner`'ı
  da (KIT-ECS §5.7) `gsb-bevy-ecs`'in `systems` modülüne gidecek
  (BACKLOG `q5:`). Bağımlılıklar: `gsb-core` + yalnız o ECS.
  `cargo tree -p gsb-core` (tüm özelliklerle) hiçbir ECS crate'i göstermez.
- **bevy_ecs:** tablo aktörün durumudur (Faz 1, A13), dünyanın değil.
  `gsb_bevy_ecs::lend(world, calls, |w| schedule.run(w))` tabloyu
  `GsbCalls` kaynağına takas eder ve koşudan sonra geri alır (iki
  `mem::swap`; ilk çağrıda kaynak yedek bir tabloyla eklenir, sonra
  tahsis yok) — sistemler `ResMut<GsbCalls>` ile `calls.call(..)` yazar.
  `Svc<T>` istemci kaynağı (`Res<Svc<BankClient>>`). Oda biçimi
  (spike'ın `BevyRoom`'u) yazılmadı: motorun `GameLogic`'i çok daha geniş;
  oyun `update_with_calls`'ında `lend` çağırır.
- **Sahip ⇄ varlık:** `owner.entity() -> Option<Entity>` (iki ECS'te de
  `Option`; `Owner::ACTOR` hiçbir zaman varlık değildir — bitleri bevy'de
  geçerli bir varlığa çözülse de), `e.to_owner()`, `owner_alive(world,
  owner)` (`GameLogic::owner_alive` için). shipyard: `EntityId::inner` ⇄
  `from_inner`, crate yok (K14).
- **Kanıt (`crates/gsb-conformance`, yayınlanmaz):** kütüphanesi
  ECS'sizdir (sözleşme, sahte banka, `Style` trait'i, `play` — gerçek
  `RoomActor`'ı q4'ün `SimRoom`'uyla adımlar); ECS'li biçimler
  `tests/calls_styles.rs`'tedir (ECS crate'leri yalnız dev-bağımlılık).
  Aynı aktarım senaryosu (rezerv, BUSY çift tık, INSUFFICIENT, tek ücret,
  `.answers`) üç biçimde bayt bayt aynı izi verir (cüzdanlar, istemci
  cevapları, banka ücretleri, `CallCounts`). KIT-ECS'in
  `gsb-kit-conformance` matrisi buraya katılabilir.
- **Göç (B152, §8):** shard göçü `PlayerMigration.calls: Exported`
  taşır. Kaynak, MIGRATE'te oturumun sahibinin (açık bağ, yoksa
  `call_owner`) çağrılarını dışa alır; `calls_exported` yalnız gönderim
  başarılınca sayılır (`take_owner` + `count_exported`), reddedilen
  gönderimde çağrılar olduğu gibi geri konur (`restore`: gönderilmiş
  olan bekleyen kümeye, taslak taslaklara; hiçbir şey sayılmaz). Hedef
  CONTROL'de oyuncuyu kurduktan sonra **kendi** `call_owner`'ı (yoksa
  oyuncu) altında `import` eder; çağrılar o tick'in FLUSH'ında aynı
  anahtarla yeniden yayınlanır (idempotent servis: tek etki), sonuç
  hedefte oyuna, `.answers` istemciye hedeften gider; kaynağa gelen geç
  cevap `results_late`. Ölü bir katılımın göçü (ayrılış önce işlendi) ve
  duran shard'da kalan göç taşıdıkları `.answers`'ı `requests_abandoned`
  sayar. Yeni sayaç yok, tel değişmedi. Kit için ayrı iş gerekmedi:
  `KitMig` `Clone` olduğundan `Exported`'ı taşıyamaz, ama çekirdek yol
  kit'in `call_owner(world, player, entity)`'si ile çalışır (hedefte yeni
  varlık sahibi) — `sharded/tests/calls_actors.rs` (B168) gerçek dört
  shard aktörüyle bunu sınar.
- **Açık bağ ve devam (B156):** park (`Detach::Hold`) oturumun cevap
  borçlarını bırakır ama açık bağı tutar (`park_session`); devam bağı yeni
  oturuma taşır (`resume_session`), oda ve shard aynı. Kayıtta iki yeni
  girdi: `Input::SessionPark`, `Input::SessionMove` (dosya etiketleri 16,
  17; eklemeli, `FORMAT_VERSION` değişmedi).
- **B163:** `RpcReply::rejected` boş gerekçeyi `REJECTED_REASON`
  (`"request rejected (no reason given)"`) yapar — eski `Reject("")` ve
  `External`'ın `Err("")`'i; sayaç aynı (`requests_rejected_logic`).
- **B169:** `Services::service` ve `RegistryParts::service`
  `Result<&mut Self, RegisterError>` döner; yinelenen tür için hiçbir şey
  başlatılmaz, ilk kayıt kalır.
- Ertelenenler BACKLOG'da `q5:` satırları.

**Faz 6 — olaylar turu** (§15'in yeniden tasarımı), sonra **istemci
çekirdeği B28 + FFI** (C#, `Forget`, üretilmiş istemciler).
