# gsb: Servis Çağrıları, Yanıtlar ve Olaylar — Tasarım (tur s0)

> **Durum:** TASARIM — kullanıcı incelemesi bekliyor. Uygulama YOK; bu
> turun çıktısı bu doküman ve `spikes/s0-calls` (çalışma alanı üyesi
> olmayan, motoru değiştirmeyen bir kanıt crate'i). Kararlar kullanıcıyla
> alındı (§2); bu doküman onları inceltir, açık kalanları §16'da
> numaralı sorular olarak sorar.
>
> Kod ve yorumlar İngilizce'dir; bu doküman Türkçe'dir (repo sözleşmesi).

**Tek paragraf özet.** Oyunun herhangi bir sistemi, herhangi bir anda,
oyunun kendi servisini (`gsb_core::service` tarzı bir aktör — gövdesi
HTTP/NATS/DB, ne olursa) **beklemeden** çağırır:
`ctx.call(&ekonomi, BuyItem{..}, ShopDone::Bought)`. Çağrı ya hemen
reddedilir (dolu kutu, pay dolu, servis yok — tipli, sayılı) ya da yola
çıkar; sonucu bir **mesaj** olarak, oyunun tipli etiketine sarılı
(`Done<ShopDone>`), kendi tick fazında — **RESPONSES**, girdiden önce —
toplu olarak mantığa teslim edilir. İstemci istekleri aynı yolu kullanır
(bugünkü `External` bunun özel durumu olur, tel aynı kalır). Sistemin
oyuncuya **ittiği** olaylar (başka odadan sohbet, arkadaş çevrimiçi,
web-mağaza bakiyesi) ayrı bir sistemdir: kimliğe adreslenir, ya doğrudan
istemciye (`PUSH` karesi) ya da oyuncunun odasının mantığına (**EVENTS**
fazı) gider. Maç dışı istekler için üyesi olmayan, simülasyonsuz bir
**hub** odası vardır; cevabı bağlantının kendi yolundan döner. Her kayıp
kendi adıyla sayılır.

```text
CONTROL → RESPONSES → READ → EVENTS → CONVERT (+istekler) → SYSTEMS → BROADCAST
```

---

## 1. Problem

Bugün motorda tick dışı iş için **tek** dikiş var:
`GameLogic::handle_request` → `RequestDecision::External(future)`
(RPC-CONTROL-PLANE §1–3). Bu dikişin üç sınırı oyunları zorluyor:

1. **Yalnız istemci isteği tetikleyebilir.** Bir sistem ("oyuncu
   zindanı bitirdi → envantere ödül yaz", "maç bitti → MMR servisine
   bildir", "her 30 sn konumu kalıcılık servisine yaz") servis
   çağıramaz; oyun ya kendi `tokio::spawn`'ını + kendi kanalını yazar
   (her oyun aynı makineyi yeniden icat eder, sayaçsız) ya da işi
   yapamaz.
2. **Sonuç oyuna dönmüyor, istemciye gidiyor.** `External`'ın future'ı
   `Bytes` üretir ve bayt doğrudan istemcinin `Private.responses`'ına
   yazılır. Dünya sonucu göremez: "satın alma başarılı → bakiyeyi düş,
   kılıcı envantere koy" mantığı servis cevabından sonra odada
   çalışamaz. Rezerv/onay (reserve/commit) yazılamaz.
3. **Sistemin bize ittiği şey için hiç yol yok.** Başka odadan sohbet,
   arkadaş çevrimiçi, davet, web mağazasından gelen bakiye değişimi —
   motor bunları ne bir odaya ne bir istemciye taşıyabilir.

Buna iki yapısal soru eşlik ediyor: (a) bağlantı tam olarak **bir**
odanın üyesi — maçtayken lobi/arkadaş listesi isteği nereye gider?
(b) İstemci SDK'ları (C#/Unity, Godot, JS, Rust) bu üç akışı (yanıt,
itme, hub) nasıl tek bir küçük ABI'den görür?

## 2. Kararlar (kullanıcıyla alındı — yeniden açılmaz)

| # | Karar | Kullanıcının gerekçesi (özet) |
|---|---|---|
| 1 | **İstemci SDK'ları:** tek Rust istemci çekirdeği; C#/Unity, Godot (gdext), JS (wasm + WebSocket), Rust için LiveKit tarzı sarmalayıcılar. Küçük, SABİT C ABI: `new(config) / command(bytes) / poll(buf, cap) -> len / free / abi_version`; protobuf `Command` ve `Event` oneof'ları. Yeni özellik = yeni proto mesajı; ABI asla değişmez. Kare başına `poll`, callback yok, buffer'ı çağıran sahiplenir. FFI yalnız istemcide. SDK kapsamı = taşıma + oturum + mesajlaşma; ECS/oyun mantığı/replikasyon politikası YOK. | Dört dil için dört protokol uygulaması bakımı imkânsız; ABI'yi sabit tutmak sarmalayıcıları ince ve yıllarca geçerli kılar; callback'ler dil sınırında thread/yeniden-giriş cehennemi. |
| 2 | **Motor ekonomi/sohbet/arkadaş/envanter TANIMLAMAZ.** Tek jenerik mekanizma; somut uygulamalar oyunun. Bayt yalnız tel/FFI sınırında; oyun kodu tipli: `trait Request { const OP: u16; type Response; type Error; }` (varsayılan prost; format değiştirilebilir), sunucu ve Rust istemcisi PAYLAŞIR — ayrışamazlar. Kayıt tekrar eden op'u reddeder; bozuk yükü motor cevaplar/sayar, oyuna asla ulaşmaz. | "Motor, oyun değil" (kit ilkesi); tip uyuşmazlığı çalışma zamanında değil derlemede yakalanmalı. |
| 3 | **ÇAĞRILAR (bizim sorduğumuz):** herhangi bir sistem, herhangi bir anda, beklemeden çağırır (`ctx.call(&service, req, tag)`). Servis bugünkü `gsb_core::service` aktör tarzı. Sonuç **mesaj** olarak, kendi tick fazında, toplu, oyunun tipli etiketiyle (`Done<Tag>`) gelir — bileşenlerde yoklanan handle'lar DEĞİL. Faz sırası: **CONTROL → RESPONSES → READ → CONVERT → SYSTEMS → BROADCAST** (yeni girdiyi okumadan önce sonuçları sindir). İstemci istekleri aynı yolu kullanır; `External` özel durum olur (tel uyumu korunur). | Aksiyonlarla aynı zihinsel model: "dış dünyadan gelen her şey bir fazda toplu içeri girer"; handle yoklamak her tick O(handle) maliyet ve dağınık kod. |
| 4 | **Uçuştaki iş koruması:** oyun çağırdığı anda bekleyen/rezerv durumunu senkron yazar (reserve/commit); motor uçuştaki tekrar-id reddini (var), bağlantı başına bekleyen cap'ini (var) ve servise idempotency anahtarı olarak kullanılabilecek **çağrı başına bir id** sağlar. | Çift harcama oyunun dünya durumuyla önlenir; motor yalnız yapı taşlarını verir. |
| 5 | **OLAYLAR (sistemin bize ittiği)** yanıtlardan AYRI bir sistemdir. Oyuncu KİMLİĞİNE adreslenir, nerede olursa olsun (herhangi bir shard'daki oda, hub, çevrimdışı). İki hedef: oyuncunun oda mantığı (dünya tepki versin — bakiye) ve/veya doğrudan istemci (odanın görmesi gerekmeyen sohbet). Ayrı faz(lar) ve ayrı istemci `Event` türü (`Event::Push`). | Yanıt korelasyonludur ve bizim başlattığımız bir şeyin sonucudur; olay korelasyonsuzdur ve dışarıdan gelir. Karıştırmak sayaçların ve sıralamanın anlamını bulandırır. |
| 6 | **Oda dışı istekler** (maçta değilken ya da maçtayken lobi/menü/arkadaş): "hub" dünyası — simülasyonsuz, düşük hızlı bir oda; her yerde aynı model. Çoklu üyelik GEREKMEMELİ: registry üzerinden yönlendir; cevap odanın private karesini değil bağlantının KENDİ çıkış yolunu kullanır. Daha iyi bir seçenek varsa işaretle. | Tek üyelik kuralı motorun en çok güvendiği değişmezlerden; hub onu kırmadan gelmeli. |

## 3. Tick: yeni fazlar ve yerleri

```text
global ticker ──TickInfo──▶ oda aktörü (tek await)
   0  CONTROL    join/leave/shutdown (değişmedi)
   1  RESPONSES  YENİ — çağrı tamamlanmaları: try_recv ile boşalt, bekleyen
                 setle uzlaştır, süre süpürmesi (yalnız uçuşta çağrı varsa),
                 boş değilse logic.on_done(batch); burada verilen istemci
                 cevapları HEMEN uzlaşır (korelasyon id'si bu tick'in READ'i
                 için serbest kalır)
   2  READ       üye başına kanal (değişmedi) + Dest::World itmelerini ayır
                 (bugünkü B103 yol işaretinin ayrıldığı gibi) + hub ise hub
                 kutusunu oku
   3  EVENTS     YENİ — bu tick'in Dest::World itmeleri; boş değilse
                 logic.on_events(batch)
   4  CONVERT    ingest(aksiyonlar), sonra tipli istekler (sıra bugünkü gibi:
                 önce aksiyonlar, sonra istekler)
   5  SYSTEMS    update
   6  BROADCAST  cevap uzlaşması + istemci-isteği süre süpürmesi + gönderim
```

**Neden RESPONSES READ'den önce (kullanıcının kararı, gerekçesi
genişletilmiş):**

- Bu tick'in girdisi, dünyanın **en güncel** hâline karşı doğrulanır.
  Spike `b_phase_order`: aynı pencerede hem "500 altın verildi" cevabı
  hem 500'lük satın alma isteği var; RESPONSES önce koştuğu için istek
  kabul edilir. Sıra ters olsaydı oyuncu "yetersiz bakiye" görür, bir
  tick sonra bakiyesi dolu olurdu.
- RESPONSES'ta verilen istemci cevabı, bekleyen istemci-isteği girişini
  **aynı anda** kapatır; böylece aynı tick'in READ'inde gelen yeniden
  deneme (aynı korelasyon id'si) "uçuşta tekrar" diye reddedilmez, taze
  istek olarak işlenir (spike `a_retried_correlation_id…`).
- Bugünkü kod zaten böyle: `phase_completions` CONTROL'den hemen sonra,
  READ'den önce koşuyor. RESPONSES onu genelleştirir — fark, sonucun
  istemciye bayt olarak değil **mantığa mesaj olarak** gitmesi.

**Neden EVENTS READ ile CONVERT arasında (kararı bu doküman veriyor):**

- `Dest::World` itmesi oyuncunun **oturumunun üyelik kanalına** biner
  (§6.2 — resume ve shard göçü boyunca oturumu yapısal olarak izleyen
  tek kanal). Kanalı fiziksel olarak okuyan READ'dir; dolayısıyla itme
  READ'den önce teslim edilemez. Ama mantığın **gördüğü** sıra yine
  "dış olgular, girdiden önce": EVENTS, CONVERT'ten önce koşar —
  web-mağaza kredisi aynı tick'teki satın alma isteğinden önce
  uygulanır (spike: `done → events → ingest → request → update`).
- RESPONSES'tan **sonra**: önce kendi başlattığımız işlerin sonucu
  yerleşir, sonra dışarıdan gelen olgular onun üstüne uygulanır. İkisi
  arasında **sıra garantisi yoktur** (farklı kanallar, §9); aynı
  servisten gelen yanıt ile olay çelişebiliyorsa oyun bir sürüm
  numarası taşımalı (ör. `balance_version`).
- Ayrı bir EVENTS fazı (RESPONSES'la birleştirilmemiş): kullanıcı kararı
  5; ayrıca anlamlar farklı — yanıt korelasyonlu ve bekleyen sete
  karşı uzlaşır, olay uzlaşmaz; sayaçları ve kayıp nedenleri ayrı.

**Boşta maliyet (spike `g_idle_cost`).** Sessiz bir odada RESPONSES =
tek boş `try_recv` + "uçuşta çağrı var mı" sayaç kontrolü; EVENTS = tek
`is_empty`. Mantık boş batch için ÇAĞRILMAZ, süpürme koşmaz, faz
tamponları hiç ayrılmaz (1000 boş tick sonrası kapasite 0). READ'in
maliyeti değişmez (itme işaretleri zaten okunan kanalda).

## 4. Sunucu API'si

### 4.1 Ortak sözleşme: `Request` ve `Event`

```rust
/// Sunucu ve Rust istemcisi AYNI crate'ten görür (oyunun "sözleşme"
/// crate'i: proto tipleri + bu impl'ler). Ayrışamazlar.
pub trait Request: WireMessage + Send + 'static {
    const OP: u16;
    type Response: WireMessage + Send + 'static;
    type Error: WireMessage + Send + 'static;
}

/// İtilen olay: bir op, bir yük, cevap yok.
pub trait Event: WireMessage + Send + 'static {
    const OP: u16;
}

/// Tel biçimi. Varsayılan özellik `prost`: her `prost::Message +
/// Default` için kapsayıcı impl. Başka bir biçim isteyen oyun özelliği
/// kapatır ve kendi tiplerine impl yazar (bkz. §16 S10).
pub trait WireMessage: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError>;
}
```

Oyun tarafı:

```rust
// game-contract crate (sunucu + istemci ortak)
impl Request for BuyItem { const OP: u16 = 1001; type Response = BuyResult; type Error = EconomyError; }
impl Event for FriendOnline { const OP: u16 = 2002; }
```

Yanlış yanıt tipi derleme hatasıdır — spike'ın `compile_fail` doc
testleri dört biçimini kilitler: servisin `Reply<BuyItem>`'a başka mesaj
göndermesi, odanın `ClientCall<BuyItem>`'a başka mesajla cevap vermesi,
etiketin mantığın `Done` tipinden başka bir şey üretmesi, servise
hizmet etmediği bir isteğin gönderilmesi (§12).

Spike'ı yazarken öğrenilen tel ayrıntısı: varsayılan değerli bir proto3
hata mesajı (`EconomyError { code: 0 }`) **sıfır bayta** kodlanır. "Yük
boş değilse oyun hatasıdır" kuralı onu motor reddi sanar. Bu yüzden
`RpcResponse`'a açık bir bayrak eklenir (`game_error = 6`, §14).

### 4.2 Çağrı ve servis tarafı

```rust
/// Odanın bir servise tutamacı. `M` oyunun kendi enum'u, servis ettiği
/// her istek için bir kol: `M: From<Call<R>>` "bu servis R'ye hizmet
/// eder" demektir.
pub struct ServiceRef<M> { name: &'static str, tx: mpsc::Sender<M> }

/// Servisin aldığı tek çağrı.
pub struct Call<R: Request> {
    pub key: CallKey,   // benzersiz; idempotency anahtarı (yeniden yayında aynı)
    pub req: R,
    pub reply: Reply<R>,
}

/// Dönüş yolu. Tipli: R'ye yalnız Result<R::Response, R::Error> ile
/// cevap verilebilir. Cevapsız düşürülürse ("öldü/unuttu/panik") odaya
/// hemen `Abandoned` gider — süre dolmasını beklemeden.
impl<R: Request> Reply<R> {
    pub fn send(self, r: Result<R::Response, R::Error>) -> Result<(), Undeliverable>;
}
```

Oyunun servisi bugünkü ekonominin aynısı, yalnız mesaj tipi değişir:

```rust
pub enum EconomyMsg { Buy(Call<BuyItem>), Grant(Call<GrantGold>), Stop }
impl From<Call<BuyItem>> for EconomyMsg { fn from(c: Call<BuyItem>) -> Self { Self::Buy(c) } }

async fn serve(mut rx: mpsc::Receiver<EconomyMsg>, http: HttpClient) {
    while let Some(msg) = rx.recv().await {          // tek await
        match msg {
            EconomyMsg::Buy(call) => {
                let http = http.clone();
                tokio::spawn(async move {
                    // Idempotency-Key: call.key → aynı anahtar = aynı sonuç
                    let r = http.buy(call.key, &call.req).await;
                    let _ = call.reply.send(r);
                });
            }
            EconomyMsg::Grant(call) => { /* … */ }
            EconomyMsg::Stop => break,
        }
    }
}
```

`CallKey` = `(origin: u64) << 64 | seq`. `origin` aktör
enkarnasyonu başına rastgele (`getrandom`, zaten bağımlılık), `seq`
monoton. Süreçler/yeniden başlatmalar arasında benzersiz; oyun onu
rezerv kaydında saklar ve yeniden yayında aynen geçer (§11).

**Çağrı fiili** (`TickCtx` üzerinde; kancalar `&TickCtx` tutar —
`kick`'in `Cell` deseni, yeniden giriş yok):

```rust
impl<'a, D: 'static> TickCtx<'a, D> {
    pub fn call<R, M>(&self, svc: &ServiceRef<M>, req: R,
                      tag: impl FnOnce(Outcome<R>) -> D + Send + 'static)
        -> Result<CallKey, Refused<R, D>>
    where R: Request, M: From<Call<R>>;

    /// Elde zaten olan bir anahtarla (göç/yeniden başlatma sonrası).
    pub fn call_keyed<R, M>(&self, svc: &ServiceRef<M>, key: CallKey, req: R,
                            tag: impl FnOnce(Outcome<R>) -> D + Send + 'static)
        -> Result<CallKey, Refused<R, D>>;

    /// Yereldeki bekleyen girişi unut (göç çıkışı); sonradan gelen cevap
    /// `calls_late`. Servis işi yine yapar — iptal değildir.
    pub fn forget(&self, key: CallKey);

    /// İstemci isteğine tipli cevap (şimdi ya da bir Done'dan sonra).
    pub fn reply<R: Request>(&self, call: ClientCall<R>, r: Result<R::Response, R::Error>);
}
```

**Etiket bir kurucudur.** `tag` sonucu oyunun mesajına çevirir; enum
tuple kolu doğrudan fonksiyondur:
`ctx.call(&econ, GrantGold { amount }, ShopDone::Granted)`. Bağlam
taşıyan çağrı kapanış kullanır:
`move |outcome| ShopDone::Bought { client, price, outcome }`. Etiket
odada kalır (servise gitmez): süre dolduğunda ya da servis düştüğünde
motor da onu çalıştırıp oyunun mesajını üretebilmelidir.

**Hızlı başarısızlık.** `call` hiçbir zaman beklemez. Sıra: uçuşta aynı
anahtar mı → (oda, servis) payı dolu mu → `try_reserve` ile kutuda yer
var mı. Yer ayırma **önce** yapılır, böylece ret isteği ve etiketi
dokunulmamış geri verir (`try_send`'in mesajı geri vermesi gibi).

### 4.3 Tamamlanma: `Outcome`, `Done`, `Refused`

```rust
pub enum CallError<E> { Service(E), TimedOut, Abandoned, Refused(RefuseReason) }
pub type Outcome<R> = Result<R::Response, CallError<R::Error>>;
pub enum RefuseReason { Full, Gone, ServiceCap, DuplicateKey }

pub struct Done<D> { pub key: CallKey, pub msg: D }

pub struct Refused<R, D> { pub reason: RefuseReason, pub req: R, /* etiket */ }
impl<R, D> Refused<R, D> {
    /// Etiketi Err(CallError::Refused(reason)) ile çalıştırır: ret, diğer
    /// her başarısızlıkla AYNI yerde ele alınabilir.
    pub fn fail(self) -> D;
}
```

**Kabul edilen her çağrı için, oda yaşadıkça, tam olarak bir `Done`**:
cevap, `Service(e)`, `TimedOut` ya da `Abandoned`. Geç cevap ikinci bir
`Done` üretemez (girişin `attempt` numarası eşleşmez → `calls_late`).
Bu, RPC-CONTROL-PLANE §1'deki "geç olay ikinci yanıt üretemez"
değişmezinin çağrıya taşınmış hâli. Ret (`Refused`) `Done` üretmez —
çağrı kabul edilmemiştir; oyun isterse `fail()` ile kendi mesajını alır.

Rezerv/onay deseni (spike `a_reserve_commit`, oyunun kodu):

```rust
fn buy(&mut self, ctx: &TickCtx<ShopDone>, client: ClientCall<BuyItem>, buy: BuyItem) {
    if self.balance - self.reserved < buy.price {
        return ctx.reply(client, Err(EconomyError { code: INSUFFICIENT }));
    }
    self.reserved += buy.price;                       // SENKRON rezerv
    let price = buy.price;
    if let Err(r) = ctx.call(&self.econ, buy, move |o| ShopDone::Bought { client, price, o }) {
        let d = r.fail();                             // ret de aynı yoldan
        self.settle(ctx, d);
    }
}
fn on_done(&mut self, ctx, done: &mut Vec<Done<ShopDone>>) {
    for d in done.drain(..) { self.settle(ctx, d.msg) }  // onay ya da geri al + istemciye cevap
}
```

### 4.4 İstemci istekleri: tipli tablo, `ClientCall`, `External`

```rust
/// Oda kurulurken bir kez doldurulur.
impl<In> RequestTable<In> {
    pub fn add<R: Request>(&mut self, wrap: fn(ClientCall<R>, R) -> In) -> Result<(), DuplicateOp>;
}
table.add::<BuyItem>(ShopReq::Buy)?;     // ikinci kayıt: Err(DuplicateOp(1001))
```

- Bilinmeyen op → motor "no handler" cevaplar; yük çözülmezse → motor
  `undecodable request payload` cevaplar, `requests_rejected_undecodable`
  sayar. İkisi de oyuna ULAŞMAZ (spike `requests.rs`).
- `ClientCall<R>` `Clone` değildir: en fazla bir kez cevaplanır
  (`ctx.reply` onu tüketir). Cevapsız düşürülebilir — bugünkü süre
  süpürmesi cevaplar (`requests_timed_out`). Bir çağrının etiketinde
  taşınarak sonradan cevaplanır: **istek, istemcinin isteği etiket
  olarak bir servis çağrısına dönüşür**, cevap gelince oda cevaplar.
- **`External` özel durum olur.** Bugünkü
  `RequestDecision::External(future)` motorun içinde "future'ı çalıştıran
  yerleşik servis + etiketi 'bu ClientCall'a baytları aynen ilet'" olan
  bir çağrıya çevrilir. Motorun kendi `Done`'ı olduğu için oyunun `D`
  tipine dokunmaz: aktör, RESPONSES fazında ikinci, motora ait küçük bir
  tabloyu (aynı `CallTable` tipi) boşaltır. İstemci aynı
  `Private.responses` girdisini görür — **tel değişmez**; bugünkü
  `requests_external`/`requests_late`/`requests_timed_out` anlamları
  korunur.
- Geçiş yolu: `handle_request(&RpcRequest) -> Option<RequestDecision>`
  (bayt) kalır; tipli tabloda bulunmayan op ona düşer, o da `None`
  derse "no handler". Oyunlar tek tek tipliye geçer.

### 4.5 `GameLogic`'e eklenenler ve göç maliyeti

```rust
pub struct TickCtx<'a, D = ()> { /* bugünküler + */ calls: Calls<'a, D>, replies: Replies<'a> }

pub trait GameLogic<W>: Send {
    // … bugünküler …
    /// Bitmiş bir çağrının oyundaki mesajı (etiketin tipi). Çağrı yapmayan: `()`.
    type Done: Send + 'static;
    /// Çözülmüş istemci isteği tipi. Tipli istek kullanmayan: `()`.
    type Req: Send + 'static;
    fn requests(&self, _t: &mut RequestTable<Self::Req>) {}
    fn on_request(&mut self, _w: &mut W, _ctx: &TickCtx<'_, Self::Done>, _r: Self::Req) {}
    fn on_done(&mut self, _w: &mut W, _ctx: &TickCtx<'_, Self::Done>, _d: &mut Vec<Done<Self::Done>>) {}
    fn on_events(&mut self, _w: &mut W, _ctx: &TickCtx<'_, Self::Done>, _e: &mut Vec<PushIn>) {}
}
```

Kancaların imzası `&TickCtx<'_, Self::Done>` olur. **Mevcut oyun
başına maliyet: iki satır** (`type Done = (); type Req = ();`); mevcut
`ctx: &TickCtx` imzaları **olduğu gibi derlenir** — varsayılan tip
parametresi `D = ()` ile normalize edilir (bu turda 1.95.0'da ayrı bir
dosyayla doğrulandı; ilişkili tip varsayılanları kararlı olmadığı için
iki satır kaçınılmaz). `RoomActor`/`ShardActor` `Done` ve `Req` için
birer jenerik kazanır; `BuiltRoom`'daki `dyn RoomLogic<…>` tipleri de.
Çağrı/tamamlama makinesi bu sefer **tek bir yardımcı tip**
(`CallTable`) olarak yazılır ve iki aktör onu paylaşır (bugünkü RPC
makinesinin iki kopyası TRAIT-ARCHITECTURE Faz 3'ün bıraktığı borç).

Kit (`gsb-kit`'in `Game` dikişleri) aynı iki ilişkili tipi ve üç kancayı
ileri taşır.

### 4.6 ECS sistemleri

`gsb-ecs`'in `System::run(world, &SystemCtx)`'i bugün `TickCtx`'i
görmez; "herhangi bir sistem çağırabilir" için sistemlerin fiile
ulaşması gerekir. Öneri (§16 S3): `SystemCtx<'a, X = ()>`'e ödünç
`pub ext: &'a X` alanı; oyun kendi `TickCtx`'ini geçirir
(`runner.run_all(world, &SystemCtx { tick, dt, ext: ctx })`). `gsb-ecs`
`gsb-core`'a bağımlı olmaz, bevy `SystemParam` makinesine girilmez.

## 5. İstemci tarafı

### 5.1 Rust çekirdeği

Çekirdek "sans-IO" bir oturum durum makinesidir (B28): AUTH/JOIN/
HEARTBEAT/LEAVE, istek tablosu, itme demux'u. **Kare başına `poll`**
iki türü asla karıştırmaz:

```rust
pub enum ClientEvent { Response(RawResponse), Push(PushFrame), /* Joined, Left, Snapshot, Error, Disconnected … */ }

let t: Ticket<BuyItem> = core.request(Target::Room, &BuyItem { .. });
// … her karede:
for ev in core.poll() {
    match ev {
        ClientEvent::Response(r) if r.id == t.id => match r.decode(&t) {
            Ok(res) => …,                                 // BuyResult
            Err(ClientCallError::Game(e)) => …,           // EconomyError
            Err(ClientCallError::Engine(reason)) => …,    // timeout/cap/no handler
            Err(ClientCallError::Mismatch) => unreachable!(),
        },
        ClientEvent::Push(p) => if let Some(f) = p.decode::<FriendOnline>() { … },
        _ => {}
    }
}
```

- Korelasyon id'leri **tek sayaçtan**, hedef (oda/hub) fark etmeksizin:
  bir bağlantıda iki hedefin id'leri çakışmaz.
- Çekirdek kendi yerel zaman aşımını ve **oturum sonunda tüm
  uçuştakileri** `SESSION_ENDED` ile kapatır: hiçbir sarmalayıcı sonsuza
  kadar bekleyen bir Promise/Task tutmaz.
- `Ticket<R>` cevabın tipini istekten alır; başka biletin baytı
  `Mismatch` döner (spike `client_mirror`).

### 5.2 Dil sarmalayıcıları (hepsi aynı `Command`/`Event` baytı üstünde)

Kayıt her dilde tiplidir ve tekrar eden op'u reddeder:
`Register<BuyItem, BuyResult, EconomyError>(op: 1001)`,
`OnPush<FriendOnline>(op: 2002, handler)`.

**C# / Unity** (C ABI üstünde; `TaskCompletionSource` `Poll` içinde,
ana thread'de tamamlanır):

```csharp
public sealed class GsbClient : IDisposable {
    readonly IntPtr h; ulong nextId; byte[] buf = new byte[64 * 1024];
    readonly Dictionary<ulong, Action<FfiResponse>> pending = new();
    readonly Dictionary<uint, Action<ByteString>> pushes = new();

    public void Register<TReq, TRes, TErr>(uint op) where TReq : IMessage<TReq> … {
        Registry.Add<TReq>(op, TRes.Parser, TErr.Parser);   // tekrar → throw
    }

    public Task<TRes> Request<TReq, TRes, TErr>(TReq req, Target target = Target.Room) {
        var reg = Registry.Get<TReq>();
        var id = ++nextId;
        var tcs = new TaskCompletionSource<TRes>(TaskCreationOptions.RunContinuationsAsynchronously);
        pending[id] = r => {
            switch (r.Status) {
                case Status.Ok:          tcs.SetResult((TRes)reg.Response.ParseFrom(r.Payload)); break;
                case Status.GameError:   tcs.SetException(new GameError<TErr>((TErr)reg.Error.ParseFrom(r.Payload))); break;
                default:                 tcs.SetException(new GsbException(r.Status, r.Reason)); break;
            }
        };
        Send(new Command { Request = new FfiRequest { Id = id, Op = reg.Op, Payload = req.ToByteString(), Target = target } });
        return tcs.Task;
    }

    public void Poll() {                       // MonoBehaviour.Update'ten, kare başına bir kez
        for (;;) {
            long n = Native.gsb_poll(h, buf, (UIntPtr)buf.Length);
            if (n == 0) break;
            if (n < 0) { buf = new byte[-n]; continue; }       // olay çekirdekte bekler
            var ev = Event.Parser.ParseFrom(buf, 0, (int)n);
            switch (ev.KindCase) {
                case Event.KindOneofCase.Response: if (pending.Remove(ev.Response.Id, out var k)) k(ev.Response); break;
                case Event.KindOneofCase.Push:     if (pushes.TryGetValue(ev.Push.Op, out var p)) p(ev.Push.Payload); break;
                // Joined / Left / Snapshot / Error / Disconnected …
            }
        }
    }
}
// var res = await gsb.Request<BuyItem, BuyResult, EconomyError>(new BuyItem { Item = "sword" });
```

`SetResult` `Poll` içinde ana thread'de çağrılır; `RunContinuationsAsynchronously`
devam kodunu Unity'nin `SynchronizationContext`'ine postalar —
`Poll`'un ortasında yeniden girişi önler, devam yine ana thread'dedir.

**Godot (gdext — Rust, C ABI gerekmez):** `GsbClient` düğümü
`_process`'te `poll` eder; `response(id, op, status, payload)` ve
`push(op, payload)` **sinyalleri**. GDScript tipli kullanım için her
istek bir `GsbCall` nesnesi döndürür (`signal completed(result)`):
`var r = await gsb.request(BuyItem.new()).completed`. Tipler godobuf
üretimi; kayıt `gsb.register(1001, BuyItem, BuyResult, EconomyError)`.

**JS (wasm + WebSocket):** çekirdek wasm'da, soket JS'te (tarayıcıda
TCP/rUDP/QUIC yok); `requestAnimationFrame` başına `client.poll()`;
`client.request(BuyItem, msg): Promise<BuyResult>` Promise'i `poll`
içinde çözülür (`reject` → `GameError<EconomyError>` ya da
`EngineError`); `client.onPush(FriendOnline, fn)`. Tipler
protobuf-es/ts-proto.

**Bevy (Rust, doğrudan):** `GsbPlugin` `PreUpdate`'te `poll` eder;
`app.add_gsb_request::<BuyItem>()` tipli kayıt (aynı `Request` trait'i —
sözleşme crate'i paylaşılır); `GsbCommands` system param'ı
`request(BuyItem{..}) -> Ticket<BuyItem>`; cevaplar
`EventReader<Responded<BuyItem>>`, itmeler `EventReader<Pushed<FriendOnline>>`.

**ABI notu (önemli):** C ABI'ye **yalnız C#/Unity** muhtaçtır. Godot
(gdext) ve Bevy Rust'tır — çekirdeğe doğrudan bağlanır; JS wasm-bindgen
kullanır. Hepsi yine de AYNI `Command`/`Event` protosunu konuşur ki
sarmalayıcıların davranışı tek bir uygunluk senaryosuyla test edilsin.
C ABI ham işaretçi ister: `unsafe`'siz yazılamaz — §16 S11.

## 6. EVENTS: sistemin bize ittiği

### 6.1 API

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

### 6.2 Yönlendirme: iki hedef, tek yol

```text
servis ──Pusher.push──▶ [registry: kimlik → canlı oturum]
                              │ yok → pushes_offline / ayrık (park) → pushes_world_parked
                              ▼
                     bağlantı aktörü (ConnIn::Push)
             Dest::Client │                    │ Dest::World
                          ▼                    ▼
              PUSH karesi (kendi yolu)   üyelik kanalı (Inbound::Push) ──▶ READ ayırır ──▶ EVENTS
              odanın haberi olmaz        oda yok → hub varsa hub'a, yoksa pushes_world_no_room
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
Defter-tabanlı teslim (park defterinin `resume_lookup(identity)`'si
zaten var) §16 S5.

**Çevrimdışı** teslim (kutu, davet saklama) motorun işi değil: servis
saklar, oyuncu giriş yapınca oyun kendi isteğiyle çeker. Motor yalnız
`pushes_offline` sayar.

**Yetenek bayrağı.** Eski bir istemci `PUSH` opcode'unu bilmez; ona
asla gönderilmez: `Auth.features` (yeni alan, §14) `PUSH` bitini
taşımayan bağlantıya `Dest::Client` itmesi `pushes_unsupported` olarak
sayılır.

**Neden yönlendirici registry (ilk sürüm):** kimlik→oturum tablosu
zaten orada (auth/close olaylarıyla güncel). Risk: sohbet hacmi kontrol
düzleminin kutusuna biner (join/leave gecikmesi). Ölç-sonra-böl: §16
S4.

## 7. Geri basınç ve adalet

- **Servis kutusu sınırlıdır**; `call` `try_reserve` ile dener, dolu →
  `Refused(Full)` + `calls_refused_full`, kapalı → `Refused(Gone)` +
  `calls_refused_gone`. Tick asla beklemez (spike `c_backpressure`).
- **(oda, servis) payı:** her oda, her servis için en fazla `k` çağrıyı
  uçuşta tutabilir (`max_in_flight_per_service`, servis başına
  geçersiz kılınabilir); aşan → `Refused(ServiceCap)` +
  `calls_refused_service_cap`. Bir oda bir servisi tek başına
  dolduramaz (spike: açgözlü oda 100 dener, 3 gönderir; ikinci oda yine
  3'ünü alır).
- **Boyutlandırma kuralı:** kutu kapasitesi `C`, payı `k` olan `N`
  oda: `N·k ≤ C` ise hiçbir oda diğerini kutuda aç bırakamaz. `N·k > C`
  iken adalet "ilk gelen" düzeyindedir (odalar tokio planlayıcısının
  sırasıyla tick'ler) — dürüst sınır budur; servis gerçekten yavaşsa
  herkes `Full` görür ve sayılır. Oda başına alt-kuyruk (servisin
  odalar arasında sırayla okuması) ertelendi: servis ya çoklu bekleme
  (yasak) ya da ek uyandırma kanalı ister (§15 A11).
- **Tamamlanma kanalı** oda başına sınırlı, kapasite ≥ 2 × oda toplam
  payı (süresi dolmuş çağrıların geç cevapları güncellerinkini
  itmesin). Yine de dolarsa: servis tarafında `Reply::send` →
  `Undeliverable::Full` (`replies_undeliverable_full`), oda tarafında
  çağrı süre süpürmesiyle `calls_timed_out` — her iki uçta adlı sayaç.
- **Tick başına çağrı bütçesi** gerekmiyor: pay, bir döngüdeki hatalı
  sistemin bile en fazla `k` çağrı göndermesini garanti eder; geri
  kalanı senkron reddedilir.
- **İstemci isteği zaman aşımı ≥ çağrı zaman aşımı + bir tick**
  olmalı (yapılandırma doğrulaması); aksi hâlde istemci, odanın hâlâ
  beklediği çağrı için "timed out" görür.

## 8. Her kaybı say: sayaç listesi

Yalnız adlar (bu turda metrik/golden/tel-magic değişikliği YOK;
bağlama s1'de). Her biri tek anlam (B32 kararı).

**Çağrı yolu (oda/shard/hub, `CallTable`):** `calls_sent`,
`calls_refused_full`, `calls_refused_gone`, `calls_refused_service_cap`,
`calls_refused_duplicate_key`, `calls_done_ok`¹,
`calls_done_service_error`¹, `calls_timed_out`, `calls_abandoned`,
`calls_late`, `calls_dropped_migrated` (`ctx.forget`),
`calls_lost_teardown`; gösterge: `calls_in_flight`.

**Servis tarafı (motorun `Reply`'ı, servise verilen sayaç seti):**
`replies_undeliverable_full`, `replies_undeliverable_gone`.

**İstemci istekleri (yeni; mevcutlar aynen kalır):**
`requests_rejected_undecodable`, `requests_replied_late` (oyun, süre
süpürmesinin zaten cevapladığı isteğe cevap verdi).

**Hub:** `hub_requests_refused_full` (bağlantı aktörü: hub kutusu
dolu — anında ret cevaplanır), `hub_requests_no_hub` (hub
yapılandırılmamış), `hub_replies_dropped_full`,
`hub_replies_dropped_closed` (bağlantının kutusu).

**İtme:** `pushes_refused_full` (Pusher → yönlendirici kutusu dolu),
`pushes_offline`, `pushes_world_parked`, `pushes_unsupported`,
`pushes_client_dropped_full`, `pushes_client_dropped_closed`,
`pushes_world_no_room`, `pushes_world_dropped_full`,
`pushes_world_dropped_closed`, `pushes_world_unbound`,
`pushes_delivered_client`¹, `pushes_delivered_world`¹.

¹ Kayıp değil, sonuç; defterin kapanması için gerekli (gönderilen =
teslim + her kayıp nedeni).

**Defter kimlikleri** (her yol için kapanmalı, loadgen'de kilitlenecek):
`calls_sent = done_ok + done_service_error + timed_out + abandoned +
dropped_migrated + lost_teardown + in_flight`;
`push (registry'ye kabul edilen) = delivered_client + delivered_world +
offline + parked + unsupported + client_dropped_* + world_no_room +
world_dropped_* + world_unbound`.

**Dürüst boşluk:** panikleyen bir oda, son metrik örneğinden sonraki
uçuştakileri `calls_lost_teardown`'a yazamaz (görev ölmüştür); operatör
son örneğin `calls_in_flight` göstergesini görür. Temiz kapanış her
zaman sayar.

## 9. Sıralama garantileri (açıkça)

1. **(oda, servis) çağrıları servise çağrı sırasıyla ulaşır** (tek
   gönderen, FIFO kutu).
2. **Cevaplar servisin bitirdiği sırayla gelir**, çağrı sırasıyla DEĞİL.
   Sıra isteyen servis tek görevde sırayla işler — o zaman uçtan uca
   FIFO'dur (tek tamamlanma kanalı). Bir tick'in batch'i varış
   sırasındadır; süresi dolanlar o tick'in cevaplarından sonra gelir.
3. **Farklı servisler arasında sıra yoktur.**
4. **Yanıtlar ile olaylar arasında sıra yoktur.** Aynı tick'te
   görülürlerse mantık önce yanıtları (RESPONSES), sonra olayları
   (EVENTS), sonra girdiyi (CONVERT) görür.
5. **Tek göndericiden tek kimliğe itmeler** düşmedikçe sırayla gelir
   (yönlendirici kutusu, bağlantı kutusu, üyelik kanalı hepsi FIFO);
   düşen itme sayılır, motor sıra numarası/boşluk algılaması vermez
   (§17).
6. **İstemci isteği cevapları:** bugünkü kural (aksiyonlar sonra
   istekler; cevaplar `Private`'ta). Hub cevapları ayrı karede
   (`RPC_RESP`): oda cevaplarıyla aralarında sıra yoktur.

## 10. Geliştirici deneyimi: sahte servis + test saati

```rust
let (econ, mut fake) = FakeService::new("economy", 8);
let mut room = Room::new(cfg, Shop::new(econ, 600), shop_table());
for _ in 0..10 {
    room.tick();
    fake.step(room.now(), |msg, s| match msg {
        // karar şimdi (anahtarla tekilleştirerek), cevap 3 tick sonra
        EconomyMsg::Buy(call) => { let r = ledger.buy(call.key, &call.req); s.after(3, move || { let _ = call.reply.send(r); }); }
        _ => {}
    });
}
assert_eq!(ledger.charges, 1);
```

- Çalışma zamanı yok, HTTP yok: servis kutusunu testin istediği anda
  okur, cevabı tick cinsinden zamanlar (`after(3, …)`), `kill()` ile
  "servis öldü"yü (`Gone` + her tutulan `Reply` → `Abandoned`)
  canlandırır.
- Saat = odanın tick sayacı (spike); motorda tick saati (F10,
  `ticker::now()`) — s1'de `gsb-kit::testing` elle ilerletilen bir
  ticker ve `FakeService`'i verir.
- Kanıtlanan senaryolar: "servis 3 tick sonra cevaplar" (çağrı t'de →
  servis t'den sonra alır → t+3'ten sonra cevaplar → mantık t+4'ün
  RESPONSES'unda görür), zaman aşımı + geç cevap, düşürülen cevap.

## 11. Göç, yeniden başlatma, kapanış

**Seçim: etiket SEYAHAT ETMEZ; cevap YENİDEN YÖNLENDİRİLMEZ; yerel giriş
sayılarak bırakılır; oyunun rezervi anahtarıyla seyahat eder ve hedef
aynı anahtarla yeniden yayınlar; servis anahtarla tekilleştirir.** Üç
durum, tek reçete:

| Durum | Motorun yaptığı | Oyunun yaptığı |
|---|---|---|
| **KitMig: varlık seam'i geçer** (çağrı uçuştayken) | Kaynak, `on_migrate_out`'ta oyunun `ctx.forget(key)`'iyle girişi bırakır (`calls_dropped_migrated`; spike'ta `CallTable::drop_for_migration`); geç cevap kaynakta `calls_late`. Motor hangi çağrının hangi varlığa ait olduğunu bilmez (etiket opak) — bırakma kararı oyunundur; unutulursa kaynak, gitmiş bir varlık için `Done` alır. | Rezerv kaydı (`price`, `key`) `ShardGame::Mig` içinde `KitMig`'le taşınır; hedef `on_migrate_in`'de `ctx.call_keyed(key, …)` yapar. |
| **Oda başka shard'a/sürece taşınır** (rehome, F32 devralma) | Bekleyen tablo taşınmaz; eski enkarnasyonun kanalı kapanır → servis `Undeliverable::Gone`. | Taşınan oda durumu rezervleri anahtarlarıyla taşır; yeni enkarnasyon ilk tick'te yeniden yayınlar. |
| **Denetim yeniden başlatması** (C1 checkpoint'ten) | Yeni `origin`, yeni kanal; eski cevaplar hiçbir zaman yeni enkarnasyona ulaşmaz (yapısal). | Checkpoint rezervleri anahtarlarıyla içerir; `restore` sonrası yeniden yayın. |

Spike `i_migration`: kaynak çağırdı, servis ücretlendirdi; kaynak girişi
bıraktı; hedef aynı anahtarla yeniden yayınladı; servis tekilleştirdi →
**tek ücret**, hedef sonucu aldı, kaynağın cevabı `calls_late`.

**Neden yönlendirme değil:** (a) etiket kapanıştır ve
`ClientCall`/yerel varlık tutamacı içerir — serileştirilemez, süreç
sınırını geçemez (DISTRIBUTED §4b); (b) A→B→C zincir göçünde ileri
işaretçi zinciri ve sona erme kuralları gerekir; (c) yeniden başlatmada
zaten imkânsız — bir mekanizma üç durumu çözüyorsa ikincisine gerek
yok. **Neden "göç bitene kadar varlığı sabitle" değil:** çağrı süresi
saniyeler olabilir; seam'de takılan varlık oyunu bozar.

**Zayıf nokta (dürüstçe):** bu reçete **idempotent servis** gerektirir
— "aynı anahtar = aynı sonuç, ikinci ücret yok". Motor bunu sağlayamaz,
yalnız anahtarı verir. İdempotent olmayan bir servis göçte çift etki
üretebilir. Belgelenen sözleşme: anahtarı saklamayan servis, göç/
yeniden başlatma altında en-az-bir-kez semantiğine razıdır.

**İstemci isteği göçte:** bugünkü duruş korunur (shard modülü: göç,
oturumun istek durumunu bırakır, `requests_abandoned`; istemci kendi
zaman aşımını görür). `ClientCall` + bekleyen istemci girişi taşınabilir
(salt veri) ama bu, kapatılmış bir kararı yeniden açar — §16 S7.

**Motorun söz verebildikleri / veremedikleri:**

- VEREBİLİR: kabul edilen her çağrı için, oda yaşadıkça, tam bir `Done`;
  hiçbir cevabın başka bir enkarnasyona/başka bir çağrıya karışmaması
  (`origin` + `attempt`); her bitişin adlı sayacı; benzersiz, kalıcı
  saklanabilir anahtar.
- VEREMEZ: tam-bir-kez **etki** (servis anahtarla tekilleştirmezse);
  çöken odanın uçuştaki çağrılarının sonucunu (C1 olmadan dünya da
  gitmiştir); iptal (`forget` yalnız yerel; servis işi yapar).

## 12. Spike: `spikes/s0-calls`

Çalışma alanı üyesi değil (`exclude`), motora dokunmaz; kendi küçük
senkron tick'ini modeller. Çalıştırma:
`cd spikes/s0-calls && cargo test` (24 test + 5 doc testi; `build.rs`
`gsb-lint`'i koşar).

**Kanıtladıkları:**

| Gereksinim | Test |
|---|---|
| (a) 5 tick'te 5 satın alma, servis 3 tick geç → tek ücret | `a_reserve_commit::five_buys_over_five_ticks_charge_once` (+ aynı id'li yeniden denemeler) |
| (b) RESPONSES READ'den önce | `b_phase_order` (iki test; tam sıra `done, events, ingest, request, update`) |
| (c) Dolu kutu hızlı başarısız + sayılı; pay adaleti | `c_backpressure` (Full, Gone, pay, rezervin aynı yoldan geri alınması) |
| (d) İtme doğru hedefe; çevrimdışı ve diğer kayıplar sayılı | `d_push` (dört test) |
| (e) Sahte servis + test saati | `e_fake_clock` (3 tick, zaman aşımı + geç, terk) |
| (f) Yanlış yanıt tipi derleme hatası | `src/lib.rs` doc testleri: 1 derlenen + 4 `compile_fail` |
| (g) Boşta maliyet | `g_idle_cost` (1000 tick: 1000 yoklama, 0 süpürme, 0 kanca, tampon kapasitesi 0) |
| Hub cevabı kendi yolundan | `h_hub` |
| Göçte anahtarla yeniden yayın | `i_migration` |
| Tipli tablo, motorun reddettikleri | `requests.rs` |
| İstemci `poll`'u iki tür + varsayılan değerli oyun hatası | `client_mirror.rs` |

**Bulgular:** (1) varsayılan değerli hata sıfır bayt → `game_error`
bayrağı (§4.1, §14); (2) RESPONSES'ta verilen cevap hemen
uzlaşmazsa aynı tick'in yeniden denemesi yanlışlıkla "uçuşta tekrar"
sayılıyordu — uzlaşma RESPONSES'un sonuna alındı; (3) "servis bu isteğe
hizmet etmiyor" hatası, tek `From` impl'inde rustc tarafından "expected
`Buy`, found `Chat`" (E0308) olarak raporlanır, E0277 değil; (4) kararlı
rustdoc `compile_fail`'de hata kodunu denetlemez — dört blok elle de
derlendi ve adı geçen kodla düştü; (5) `TickCtx<'a, D = ()>` +
`type Done = ()` mevcut `&TickCtx` imzalarını kırmıyor (ayrı dosyada,
1.95.0).

**Kanıtlayamadıkları:** gerçek aktör atlamaları (registry, bağlantı
aktörü — spike'ta `Directory::route` bir fonksiyon); asenkron servis
çalışma zamanı; gerçek tel baytları (`wire.rs` elle yazılmış ayna);
ayırma sayımı (sayan `GlobalAlloc` `unsafe` ister — yapısal kanıtla
yetinildi); sharded oda ve shard göçünün kendisi (göç `drop_for_migration`
+ ikinci oda ile modellendi); çok süreç; ABI ve dil sarmalayıcıları;
gerçek yük altında maliyet (loadgen s1'de).

## 13. Hub

**Ne:** üyesi olmayan bir `RoomActor` (aynı faz sırası, aynı
`GameLogic`), düşük hızda (ör. 10 Hz; yapılandırılabilir), simülasyonu
olmayabilir. Durumu kimlik başınadır (parti, lobi, menü) — kalıcı olanı
oyunun servisindedir.

**Yönlendirme (çoklu üyelik yok):**

1. İstemci `RpcRequest { target: HUB }` gönderir (yeni alan; 0 = oda,
   bugünkü anlam).
2. Bağlantı aktörü (auth'lu olmalı; değilse bugünkü auth ihlali yolu)
   `HubRequest { conn, identity, envelope, reply_to: kendi ConnIn
   kutusu }`'yu hub'ın istek kutusuna `try_send` eder. Kutu dolu → aktör
   **kendisi** `RPC_RESP` reddi yazar, `hub_requests_refused_full`. Hub
   tutamacı registry'den auth başarısında gelir (`Seat`'in yanında).
3. Hub READ'de kutuyu bütçeli okur, tipli tabloyla çözer, bekleyen
   istemci girişini **bağlantı anahtarıyla** tutar (bağlantı başına
   cap + uçuşta tekrar reddi aynen).
4. Cevap `ConnIn::Deliver(RpcResp)` ile bağlantı aktörüne, o da `RPC_RESP`
   (yeni base opcode) karesi yazar. Bağlantı aktörü üzerinden (yazıcıya
   doğrudan değil) çünkü oturum durumunun (kapanıyor mu, yetenek
   bayrakları) otoritesi odur ve kendi sayaçlarını tutar.
5. Bağlantı kapanırsa hub'a haber verilmez: girişler zaman aşımıyla
   düşer, geç cevap `hub_replies_dropped_closed`.

**`Dest::World` itmesi, odası olmayan oyuncu için** hub'a gider
(hub yapılandırılmışsa) — "oyuncunun dünyası" maç dışında hub'dır;
yoksa `pushes_world_no_room` (§16 S6).

**Daha iyi bir seçenek var mı (istenen işaret):** değerlendirilen üç
alternatif:
(a) **Hub = tick'siz düz bir oyun servisi** (registry `target: HUB`'ı
oyunun `ServiceRef`'ine yollar). Daha az gecikme (tick beklemesi yok),
ama ikinci bir programlama modeli — kullanıcı "her yerde aynı model"i
seçti; tick'li hub'ın bedeli bir hub tick'i (≤100 ms @10 Hz), menüler
için kabul edilebilir. **Elenmedi, not edildi:** hub mantığı tamamen
durumsuz kalırsa (a) daha basittir.
(b) **Lobi odasına normal üyelik + maçtayken lobiye istek yok:** bugün
mümkün ama maç içi arkadaş listesi/davet isteğini çözmez.
(c) **İkincil üyelik:** reddedildi (karar 6).
Sonuç: kullanıcının hub'ı + bağlantının kendi yolu; daha iyi bir seçenek
bulunmadı.

**İsim çakışması:** `registry/hub.rs` (`TeamHub`, takım-export rölesi,
CROSS-SHARD §8b) zaten "hub". Öneri: yenisi `Hub` adını alır, eskisi
`team_hub.rs`'e taşınır (crate-içi; dış API'ye dokunmaz) — §16 S15.

## 14. Tel değişiklikleri (eklemeli — DESIGN §5)

Hepsi alan/opcode/mesaj **ekler**; hiçbir alan numarası silinmez ya da
anlamı değişmez; `PROTOCOL_VERSION` artmaz (tam eşitlik politikası eski
istemcileri reddederdi — DESIGN §5.5).

```proto
// base.proto (eklemeler)
message Auth {
  // … 1..3 aynen …
  // İstemcinin anladığı isteğe bağlı kareler (bit alanı). 0 = hiçbiri
  // (eski istemci). bit 0 = PUSH. (RPC_RESP bit istemez: yalnız
  // target = HUB yollayan — yani yeni — istemciye gider.)
  uint32 features = 4;
}

message RpcRequest {
  uint64 id = 1; uint32 op = 2; bytes payload = 3;
  // 0 = ROOM (bugünkü anlam, eski istemci bunu yollar), 1 = HUB.
  uint32 target = 4;
}

message RpcResponse {
  uint64 id = 1; bool ok = 2; uint32 op = 3; string reason = 4; bytes payload = 5;
  // ok=false iken: true → payload oyunun tipli hatası (Request::Error);
  // false → motor reddi, metin `reason`'da (bugünkü tek şekil).
  bool game_error = 6;
}

// Sunucu → istemci, opcode PUSH = 18. Oyun olayının op'u ve yükü.
message Push {
  uint32 op = 1;
  bytes payload = 2;
}
```

- **Yeni base opcode'lar:** `RPC_RESP = 17` (yükü `RpcResponse` —
  DESIGN §5.2'nin elenen alternatifi 3 "RpcResponse'a kendi opcode'u"
  burada farklı gerekçeyle geri gelir: hub'da binilecek bir `Private`
  karesi YOK; oda cevapları `Private.responses`'ta kalmaya devam eder),
  `PUSH = 18`. İkisi de güvenilir band (≤64; kayıp = bekleyen istemci
  / kaçan olay — DESIGN §5.1 tablosu).
- **Eski istemci:** `target` yollamaz (0), `features` yollamaz (0) →
  ona `PUSH`/`RPC_RESP` asla gönderilmez; `game_error`'ı okumaz ama
  `ok=false` görür (bugün de "reason boş ret" mümkündü). Uyum testi s1'de
  `wire_contract.rs`'e eklenir.

**`ffi.proto` taslağı** (istemci çekirdeği ↔ sarmalayıcı; telde değil):

```proto
syntax = "proto3";
package gsb.ffi;

message Config {
  string url = 1;               // ws://, tcp://, udp://… (B28 kapı seçimi)
  string name = 2;              // yerel auth
  bytes ticket = 3;             // bilet auth
  bytes server_key = 4;         // rUDP sabitlenmiş anahtar (grant)
  uint32 request_timeout_ms = 5;// çekirdeğin yerel zaman aşımı
}

enum Target { TARGET_ROOM = 0; TARGET_HUB = 1; }

message Command {
  oneof kind {
    JoinRoom join = 1;          // { uint64 room }
    LeaveRoom leave = 2;
    SendAction action = 3;      // { uint32 op; bytes payload } — ateşle-unut
    Request request = 4;        // { uint64 id; uint32 op; bytes payload; Target target }
    Disconnect disconnect = 5;
  }
}

enum Status {
  STATUS_OK = 0;
  STATUS_GAME_ERROR = 1;        // payload = Request::Error
  STATUS_ENGINE_REJECTED = 2;   // reason = motorun metni
  STATUS_LOCAL_TIMEOUT = 3;     // çekirdeğin kendi süresi
  STATUS_SESSION_ENDED = 4;     // oturum bitti; uçuştaki her istek bununla kapanır
}

message Response { uint64 id = 1; uint32 op = 2; Status status = 3; string reason = 4; bytes payload = 5; Target target = 6; }
message Push     { uint32 op = 1; bytes payload = 2; }

message Event {
  oneof kind {
    Connected connected = 1;    // { string player; uint64 pinned_room }
    Joined joined = 2;          // { uint64 room; uint64 entity }
    Left left = 3;
    Frame frame = 4;            // oyun bandı kare: { uint32 op; bytes payload } (snapshot, Private'ın oyun kısmı)
    Response response = 5;
    Push push = 6;
    ServerError error = 7;      // { uint32 code; string message } (ErrorCode)
    Disconnected disconnected = 8; // { string reason; bool by_server }
  }
}
```

C ABI (sabit, beş fonksiyon):

```c
uint32_t gsb_abi_version(void);
void*    gsb_new(const uint8_t* config, size_t len);       // NULL = bozuk config
int32_t  gsb_command(void* c, const uint8_t* cmd, size_t len); // 0 = kabul; <0 = bozuk / kuyruk dolu
int64_t  gsb_poll(void* c, uint8_t* buf, size_t cap);      // >0 = bir Event'in uzunluğu; 0 = yok;
                                                            // <0 = -gereken; olay çekirdekte bekler
void     gsb_free(void* c);
```

İstek id'sini sarmalayıcı seçer (kendi `TaskCompletionSource` haritası
için); çekirdek uçuşta tekrarı yerel olarak `STATUS_ENGINE_REJECTED`
ile reddeder.

## 15. Elenen alternatifler

| # | Alternatif | Neden elendi |
|---|---|---|
| A1 | Çağrı başına handle (future/promise) bileşende saklanır, sistemler yoklar | Kullanıcı kararı 3; ayrıca her tick O(handle) yoklama, toplu teslim yok, kayıp sayımı dağınık. |
| A2 | Tick içinde `await`/`block_on`/yerel executor | Tek-await kuralı; senkron tick'in tüm maliyet garantileri gider. |
| A3 | Etiketi servise gönder, servis geri yollasın | Zaman aşımı ve servis ölümünde etiket kaybolur → oyunun rezervi sonsuza kadar kilitli kalır; servis oyun tipini bilmek zorunda kalır. |
| A4 | Tipi silinmiş etiket (`Box<dyn Any>`) + oyunun çalışma zamanında downcast'i | Derleme zamanı güvencesi kaybolur (karar 2). Spike'ta silme yalnız motorun içinde; `attempt` eşleşmesi doğru tipi garanti eder. |
| A5 | Etiket değer + `done.outcome::<R>()` erişimcisi | Yanlış `R` ile okuma çalışma zamanı hatası; kurucu-kapanış hem tipli hem tek kavram. |
| A6 | Tamamlamaları bağlantı kutusundan dolaştır | RPC-CONTROL-PLANE §2 alternatif 2 ile aynı nedenle (ikinci sahip, ek atlama). |
| A7 | `Dest::World` için oda başına itme kutusu + kimlik→oda | Sharded odada sahiplik yayını; geçiş anındaki kayıp sayılamaz (§6.2). |
| A8 | RESPONSES + EVENTS tek "gelen kutusu" fazı | Kullanıcı kararı 5; anlam, uzlaşma ve sayaçlar farklı. |
| A9 | Göçte cevabı yeni shard'a yönlendir | Etiket serileştirilemez, zincir göç, yeniden başlatmada imkânsız; anahtarla yeniden yayın üçünü birden çözüyor (§11). |
| A10 | Motor kalıcı bir giden kutusuyla tam-bir-kez etki | C1 kapsamı; motor en-çok-bir-kez `Done` + anahtar verir, tam-bir-kez etki idempotent servisin işi. |
| A11 | Servis içinde oda başına alt kuyruk (adil okuma) | Çoklu bekleme yasak; uyandırma kanalı ek kavram. Pay + boyutlandırma kuralı ilk sürüme yeter; ölçüm gösterirse dönülür. |
| A12 | Sınırsız servis kutusu | Geri basınç yok; yavaş servis belleği yer. |
| A13 | Kapasite için bekle (`send().await`) | Senkron tick'te imkânsız. |
| A14 | `PROTOCOL_VERSION` artışıyla `PUSH`'u aç | Tam eşitlik politikası bütün eski istemcileri reddeder; `Auth.features` eklemeli. |
| A15 | "Yük boş değilse oyun hatası" kuralı | Varsayılan değerli hata sıfır bayt (spike bulgusu) → `game_error` bayrağı. |

## 16. Açık sorular (her biri öneriyle)

1. **S1 — Etiketin şekli.** Kurucu-kapanış (`impl FnOnce(Outcome<R>) -> D`,
   spike) mi, değer + tipli erişimci mi? **Öneri:** kurucu-kapanış (tam
   tipli, tuple kolu doğrudan kurucu). Bedel: çağrı başına bir `Box`.
2. **S2 — `GameLogic`'e iki ilişkili tip** (`Done`, `Req`; mevcut oyun
   başına 2 satır) mi, ayrı bir opt-in alt-trait mi? **Öneri:** ilişkili
   tipler; alt-trait aktörde ikinci bir `dyn` yolu ister.
3. **S3 — ECS sistemlerinin fiile erişimi.** **Öneri:** `SystemCtx<'a, X = ()>`'e
   `ext: &'a X`; oyun `TickCtx`'ini geçirir.
4. **S4 — İtme yönlendiricisi registry'de mi, ayrı aktörde mi?**
   **Öneri:** registry'de başla (kimlik tablosu orada); loadgen'e sohbet
   modu ekle; join gecikmesi yükselirse ayrı `PushRouter` aktörüne böl.
5. **S5 — Park edilmiş oyuncuya `Dest::World` itmesi.** **Öneri:** s1'de
   `pushes_world_parked` olarak say; defter-tabanlı teslim (resume
   yayını deseni) gerçek bir oyun isteyince.
6. **S6 — Odası olmayan oyuncuya `Dest::World`.** **Öneri:** hub
   yapılandırılmışsa hub'a, değilse `pushes_world_no_room`.
7. **S7 — Göçte istemci isteği.** Bugünkü "bırak + `requests_abandoned`"
   mı, `ClientCall` + bekleyen girişin `KitMig`'le taşınması mı?
   **Öneri:** bugünkü duruş; onayı oyun bir `Dest::Client` itmesiyle
   verir ("satın alma tamamlandı").
8. **S8 — Hub sayısı.** **Öneri:** süreç başına bir hub; kimlik-hash'li
   bölme DISTRIBUTED turuna.
9. **S9 — Yetenek pazarlığı.** **Öneri:** `Auth.features` bit alanı.
10. **S10 — Format değiştirilebilirliği.** **Öneri:** `WireMessage` +
    varsayılan `prost` özelliğiyle kapsayıcı impl; başka biçim = özelliği
    kapat, tipe impl yaz.
11. **S11 — FFI'da `unsafe`.** C ABI ham işaretçi okur; proje `unsafe`
    yasaklıyor. **Öneri:** tek, küçük, denetlenen `gsb-ffi` crate'i
    (beş fonksiyon, `#![deny(unsafe_op_in_unsafe_fn)]`, her blokta
    `SAFETY:` yorumu, fuzz testi) için açık, belgelenmiş istisna;
    alternatifi uniffi (üretilen kod yine `unsafe`, kontrol bizde değil).
12. **S12 — Servis adaleti payın ötesinde.** **Öneri:** pay +
    boyutlandırma kuralı; alt kuyruk ölçümle.
13. **S13 — Çağrı süresi nerede ayarlanır?** **Öneri:** servis başına
    varsayılan (`ServiceRef::with_timeout`), çağrı başına geçersiz
    kılma; oda genelinde tek değer yok.
14. **S14 — Çok süreçli itme.** Kimlik başka süreçteyse yerel
    yönlendirici `pushes_offline` der (yanlış anlam). **Öneri:** ilk
    sürümde oyunun veri yolu (NATS) itmeyi doğru sürece taşır; motor
    tarafında `pushes_not_here` adı DISTRIBUTED turunda.
15. **S15 — "hub" isim çakışması** (`TeamHub`). **Öneri:** yeni kavram
    `Hub`; `registry/hub.rs` → `registry/team_hub.rs`.
16. **S16 — İstemci çekirdeğinin yerel zaman aşımı.** **Öneri:** evet,
    istek başına (`Config.request_timeout_ms` varsayılanı) + oturum
    sonunda hepsini `SESSION_ENDED` ile kapat.

## 17. Hâlâ eksik olanlar (bu tasarımın kapsamadığı)

- **Varlık (presence) beslemesi:** arkadaş servisi "X çevrimiçi" için
  auth/close olaylarını ister. Registry'den oyunun servisine bir
  `PresenceSink` (match-result sink'inin ikizi) doğal; bu turda yok.
- **Çevrimdışı kutu / davet saklama:** oyunun servisi.
- **Odaya/herkese yayın itmesi** (kimlik değil oda/sunucu adresli
  duyuru) — kimlik adresli `push_many` dışında yok.
- **Kimlik başına itme hız sınırı** (bir sohbet spamcısının kurbanın
  bağlantı kutusunu doldurması): servisin işi; motor yalnız sayar.
- **İstemci tarafı boşluk algılama** (itmelerde sıra numarası) yok.
- **İptal:** `forget` yereldir; servise iptal sinyali yok.
- **Hub'a özel op tablosu ve yetkilendirme** (hangi op'lar yalnız hub'da;
  maç içinden hub'a hız sınırı) — s1c'de belirlenecek.
- **Metrik/golden/loadgen bağlantısı:** bu turda bilerek yok.
- **Oyun mesajlarının sürümlenmesi** (yeni alan eklemeli; yeni op'u
  bilmeyen eski sunucu "no handler" der) — oyunun kararı (motor, oyun
  değil).
- **wasm'da taşıma:** yalnız WebSocket; rUDP/QUIC tarayıcıda yok.
- **Sözleşme crate'i deseni** (sunucu + istemci aynı `Request` impl'leri):
  `gsb-demo`'nun proto'su bugün sunucu crate'inin içinde; s1'de bir
  `gsb-demo-contract` örneği gerekecek.

## 18. Uygulama planı

**Faz 1 — B28: istemci çekirdeği** (sunucu değişmez)
- `gsb-client`'ı `gsb-core`/`gsb-net` sunucu tarafından ayır (`UdpClient`
  bölmesi — B28'in kendisi); sans-IO oturum çekirdeği (`gsb-client-core`):
  AUTH/JOIN/HEARTBEAT/LEAVE, istek tablosu (tek id sayacı), `poll` ile
  `ClientEvent`; wasm derleme kapısı.
- Ortak sözleşme: `Request`/`Event`/`WireMessage` küçük bir crate'te
  (`gsb-contract`, ya da `gsb-protocol` içinde) — sunucu ve istemci
  buna bağlanır.
- `ffi.proto` `gsb-protocol`'de tanımlanır; çekirdek `Command`/`Event`
  konuşur (henüz C ABI yok).
- Tel: istemci yalnız bugünkü baytları yollar; `RPC_RESP`/`PUSH`
  çözücüleri var ama sunucu henüz göndermez.

**Faz 2 — s1: sunucu tarafı** (üç alt tur)
- **s1a — çağrılar + RESPONSES + tipli istekler** (tel değişikliği yok):
  `CallTable` (oda/shard/hub ortak), `TickCtx<'a, D = ()>`,
  `GameLogic::{Done, Req, requests, on_request, on_done}`,
  `phase_completions` → RESPONSES, `External`'ın motor tablosuna
  taşınması, `ServiceRef`/`Call`/`Reply`, servis sayaçları,
  `gsb-ecs` `SystemCtx::ext`, kit dikişleri, `gsb-kit::testing`'e
  `FakeService` + elle ilerletilen ticker, demo ekonomisinin `Call<R>`'ye
  geçişi, sayaç adları (metrik bağlantısı ayrı küçük paket).
- **s1b — itmeler + tel:** `base.proto` eklemeleri (`Auth.features`,
  `RpcResponse.game_error`, `Push`, `PUSH`), `wire_contract.rs` kilitleri,
  registry'de kimlik dizini + `Pusher`, `ConnIn::Push`/`Deliver`, READ'de
  itme işareti ayırma, EVENTS fazı, `on_events`, sayaçlar.
- **s1c — hub:** `RpcRequest.target`, `RPC_RESP`, bağlantı aktöründe hedef
  yönlendirmesi, hub tutamacının auth'ta verilmesi, `[hub]` config'i,
  hub'a `Dest::World` düşüşü, `TeamHub` yeniden adlandırması.
- Her alt turun kapıları: hedefli testler ajanda, tam set ebeveynde;
  loadgen'e çağrı/itme/hub modları (ölçüm turu).

**Faz 3 — FFI + dil paketleri**
- `gsb-ffi` (C ABI, S11 kararına göre), C#/Unity UPM paketi (üretilmiş
  protobuf + `GsbClient`), Godot gdext eklentisi (sinyaller + GDScript
  `GsbCall`), JS wasm paketi (Promise), Bevy eklentisi (Events).
- Tek **uygunluk senaryosu** (bağlan → katıl → istek → itme → hub → kopuş)
  her sarmalayıcıya karşı aynı sunucuyla koşar.
