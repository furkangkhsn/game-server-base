# gsb: Güvenlik Turu Tasarımı — TLS, Rate-Limit, Pre-Auth Sınırlar

> Durum: ~~TASARIM~~ UYGULANDI (iki uygulama turu bu dokümanı sözleşme
> aldı: A=TLS, B=sınırlar; sonraki turların eklemeleriyle durumlar §1
> tablosunda). Dış inceleme ailesinin kalan teknik maddeleri burada
> kapanır: şifreleme, auth rate-limit, pre-auth tahsis sınırı.

## 1. Kapsam ve statü

| Madde | Tur | Durum |
|---|---|---|
| TCP üstüne TLS (rustls) | Tur A | ✅ Uygulandı |
| Auth rate-limit + pre-auth amplifikasyon sınırı | Tur B | ✅ Uygulandı |
| Pre-auth oturum tahsis sınırı | Tur B | ✅ Uygulandı |
| Post-auth HEARTBEAT_ACK kısması (§3.2'nin ikinci yarısı) | Bağlantı sınırları turu | ✅ Uygulandı |
| Tıkanmış yazmaya süre sınırı (`write_stall_secs`) | Bağlantı sınırları turu | ✅ Uygulandı (§3.5); **bayt-granüler** (stall gözlemlenebilirliği turu) |
| Sunucu-başlatımlı kapanış sayaçları, sebep bazında | Stall gözlemlenebilirliği turu | ✅ Uygulandı (§3.6) |
| WS kapısının RFC 6455 uyumu (parça arası veri çerçevesi, uzunluk kodlaması, kapanış kodları) + CI'da Autobahn kapısı | WS uyum kapısı turu | ✅ Uygulandı (§3.7); ~~Autobahn işi yerelde koşulmadı~~ *(yerelde koşuldu — `crossbario/autobahn-testsuite:25.10.1`, 98 vaka, `check` geçti; §3.7 "Autobahn kapısı")* |
| rUDP cookie rotasyonu (yakalanan proof'un son kullanma tarihi) | rUDP doğruluk turu | ✅ Uygulandı (DESIGN §6, "Cookie rotasyonu"; slot = 10 sn, pencere 10-20 sn) |
| rUDP parçalama: yeniden birleştirme yalnız istemcide, sabit sınırlı; sunucu istemci parçasını reddeder | rUDP parçalama turu (U) | ✅ Uygulandı (§4.1; DESIGN §6 "MTU") |
| rUDP el sıkışma kaybı: proof yeniden gönderimi + kabul (`ACK{1}`), sunucu proof'ta idempotent | H turu | ✅ Uygulandı (§4.2; DESIGN §6 "El sıkışma kaybı") |
| Doğrulanmış kimlik = karakter anahtarı (ticket'sız yol yalnız geliştirme) | K4 turu | ✅ Uygulandı (§4b) |
| El sıkışma accept döngüsünün dışında, kapı başına sınırlı (WS/TLS/QUIC; sessiz tek soket kapıyı kilitliyordu) | B31 | ✅ Uygulandı (§4.3; DESIGN §6 "El sıkışan kapılar") |
| rUDP şifreleme/congestion | Kapsam DIŞI — rUDP deneysel statüde; kanıtlanmış taşıma ya da ayrı tur |
| Admin HTTP auth | OPS.md NOT-DONE (localhost sözleşmesi) |
| Ops HTTP istek başlığına süre sınırı (sessiz / damlatan eş görevini tutmaz; 5 sn, sonra tek `408`) | B47 | ✅ Uygulandı (OPS §3 "İstek başlığının süre sınırı"); eşzamanlı ops bağlantı tavanı ve yanıt yazmanın süre sınırı yok (§6) |

## 2. TLS (Tur A)

### Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **rustls**, native-tls değil | Saf Rust, ring/aws-lc arka planı; platform OpenSSL bağımlılığı yok — projenin bağımlılık disiplinine uyar |
| 2 | Yeni transport: `TlsTransport` (`gsb-net`) | `Transport`/`Listener`/`Endpoint` soyutlamasının ilk gerçek karşılığı: accept sonrası TLS handshake, ardından MEVCUT uzunluk-prefix framing değişmeden akar. Aktör katmanına sıfır dokunuş |
| 3 | Config: `tls_cert`, `tls_key` (PEM yolları). İkisi boş = plaintext (varsayılan, davranış değişmez); tekisi dolu = başlatma HATASI | Sessiz zayıf geri düşüş yok — rUDP cookie-key kararının aynı ilkesi |
| 4 | İstemci tarafı: loadgen/example'a `--tls-ca <PEM>` bayrağı | Self-signed senaryo dahil uçtan uca doğrulanabilsin |
| 5 | Test sertifikaları: `rcgen` **yalnız dev-dependency** ile koşu-anında üretilir | Repoya sertifika kommitlemek yerine her test kendi mini-PKI'sını kurar; runtime bağımlılığı sıfır kalır |
| 6 | ALPN/ad doğrulama: istemci server adını config'den alır (`--tls-server-name`, varsayılan "localhost") | Test sertifikaları localhost SAN'lı üretilir |
| 7 | rUDP TLS almaz | Deneysel statü (udp.rs beyanıyla tutarlı); `transport = "udp"` + tls_cert birlikte istenirse başlatma hatası |

### Uçlar

- Handshake yavaş/düşmanca istemci: TLS accept timeout (config'siz sabit,
  örn. 10 sn) aşımında bağlantı kapatılır — reader-pump idle idiom'u ile
  aynı aile. *(B31: süre sınırı yalnız o bağlantının yuvasını tutar;
  el sıkışma artık accept döngüsünde değil, bağlantı başına görevde ve
  kapı başına sınırlı sayıda — §4.3.)*
- `TlsTransport` plaintext `tcp` ile aynı e2e süitinden geçer
  (parametreli: her guardrail testi iki transportta da koşar — rUDP'de
  yapılanın TLS karşılığı)
- Performans: loopback ölçümü yük turu geleneğine göre ROADMAP'e yazılır
  (beklenti: el sıkışma maliyeti bağlantı başina bir kez; tick yolu
  değişmez)

## 3. Rate-limit + amplifikasyon sınırı (Tur B)

### Kararlar

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **AUTH deneme limiti:** bağlantı başına 10 sn pencerede 3 deneme; aşımı HARD-violation (bütçe puanı) | Mevcut ihlal-bütçesi makinesinin yeniden kullanımı — yeni mekanizma yok; meşru istemci 3 denemede asla aşmaz |
| 2 | **HEARTBEAT yanıtı (her iki faz):** en fazla 1/sn cevap; fazlası sessizce sayılır (RACE-violation değil, sayaç) | HEARTBEAT 1:1 cevap amplifikasyonunun kapatılması. **GÜNCELLENDİ** (bağlantı sınırları turu): kural artık auth sonrasını da kapsıyor — aşağıya bakınız |
| 3 | **Pre-auth toplam frame bütçesi:** auth başarısına kadar toplam N=64 frame; aşımında bağlantı kapanır (ERROR 9) | Auth etmeden sonsuz kontrol-frame üretebilmenin kapatılması; N meşru el sıkışmayı (AUTH+JOIN+heartbeat'ler) fazlasıyla karşılar |
| 4 | Hepsi bağlantı actor'ünün yerel durumunda — kilit/kanal eklenmez | Mevcut violation-budget ile aynı desen |

### 3.2'nin ikinci yarısı — post-auth kısma (bağlantı sınırları turu)

Tur B'nin kararı yalnız `WaitingAuth` fazına bağlıydı; auth başarısından
sonra her heartbeat koşulsuz cevaplanıyordu, yani mimarinin tek 1:1
gelen→giden dönüşümü kimliği doğrulanmış istemcilere sınırsız açıktı.
Aynı mekanizma sınırın ötesine taşındı — yeni bir makine değil, aynı
eşik.

**Neden istemciye görünen semantik değişmiyor:** düzgün bir istemci
saniyede bir heartbeat atar, yani eşiğin kendi temposundadır; her
cevabını ve `HeartbeatAck.tick`'ten okuduğu RTT'yi olduğu gibi alır.
Saniyede binlerce atan bir istemci aralık başına bir cevap alır, gerisi
sayılır.

**Canlılıkla etkileşim (doğrulandı, teste kilitlendi):** idle penceresini
sıfırlayan şey frame'in GELMESİDİR, cevabı değil (`pump.rs` her okumayı
yeniden sarar). Yani cevapsız bırakılan bir heartbeat göndericisini asla
"sessiz" göstermez. Kilit:
`e2e.rs::active_heartbeat_survives_the_idle_window` — eşiğin
cevapladığından dört kat hızlı heartbeat atan istemci
beş saniye boyunca hiç kapatılmıyor, ve ack sayısı İKİ taraftan da
doğrulanıyor (hâlâ cevaplanıyor; artık gönderim hızıyla 1:1 değil).
İstemci bir sonraki heartbeat'inden fazla beklemez ve kendi en büyük
gönderim boşluğunu ölçer: pencere kadar susmuş bir istemcinin
kapatılması (donan bir koşu) sunucu hatası değil, açık mesajla
"kanıtsız koşu"dur (BACKLOG F52).

**Kısmanın testleri yapıyla (F52).** `security.rs`'nin üç
heartbeat testi ve `conn_counts/heartbeats.rs` "bir ACK, sonra 400 ms
sessizlik" okumaz: patlamanın ne aldığı bir ÇİTE kadar okunur (AUTH
sonucu, kaydı olmayan JOIN'in puansız `ERROR`'u, aktörün sonu — aktör
onları ancak önündeki her kareyi işledikten sonra yollar) ve cevaplar
kısmanın kendi sınırına göre sayılır: cevaplanan heartbeat'ler en az bir
aralık arayla, yani `span` içinde işlenen patlama en çok `1 + ⌊span /
1 sn⌋` cevap alır — takılmasız koşuda tam bir. Sayaç testi her
heartbeat'in ya cevaplandığını (ACK kendi tick'ini yankılar) ya da kendi
fazının sayacında sayıldığını kesin eşitlikle sınar. Bir saniyelik
takılma elle sokulunca eski testler 3/3 düştü, yenileri 3/3 geçti.

**Saat tek, sayaç iki.** Saat tek çünkü kısma tek bir hız sınırlayıcıdır;
auth başarısında BİR kez sıfırlanır (§3.3 frame bütçesini emekliye ayıran
aynı faz sınırı), bu da bağlantı ömrü boyunca tam olarak bir fazladan
cevap eder (AUTH bir kez başarılı olur) ve "authenticated oturumun ilk
heartbeat'i her zaman cevaplanır" garantisini verir. Sayaçlar ayrı çünkü
farklı soruları yanıtlarlar: pre-auth fazlalık §3.2'nin GÜVENLİK sinyali
(yanında §3.3 bütçesi ve §4 cap'i vardır, çare bir pre-auth guardrail'i
sıkmaktır), post-auth fazlalık ise BİLİNEN bir istemcinin heartbeat
zamanlayıcısının bozuk olduğunu söyler — bir hata raporu, bir saldırı
değil.

**Sayaçlar artık metrikte (B56, sayım turu 2).** İki sayaç
(`m_preauth_hb_extra`, `m_hb_extra`) bağlantı aktöründe sayılıyor ama
yalnız debug satırına yazılıyordu: operatör bu sinyali göremiyordu.
Artık örneklerde delta olarak taşınıyor
(`ConnSample`/`NetReport::heartbeats_throttled_preauth`,
`::heartbeats_throttled_authed`; faza göre ayrı, çünkü soruları ayrı):
`gsb-metric scope=net` satırında `requests_no_room=`'dan sonra
`hb_throttled_preauth= hb_throttled_authed=`, Prometheus'ta
`gsb_net_heartbeats_throttled_{preauth,authed}_total`, OTLP'de
`_total`'sız, loadgen telinde `GSMN`, `RESULT`'ta aynı anahtarlar. İhlal
değiller (`violations` kıpırdamaz). Aktörün kendi ömür sayaçları debug
satırı için kalır; örnekler son akıştan beri olan farkı taşır. Kilit:
`gsb-core/tests/conn_counts/heartbeats.rs` (kimlik doğrulamadan önce
dört, sonra üç heartbeat → 3 ve 2, iki ara akışla).

**Bütçeye yazılmıyor** (pre-auth gerekçesinin aynısı): kısma zaten
maliyeti sınırlıyor, dolayısıyla puanlamak düşman tarafında hiçbir şey
kazandırmaz; yalnız dürüst-ama-hatalı istemciyi (gevşek bir NAT
keepalive'ı) zorla düşürür.

## 3.4. Post-auth geçerli girdinin HACMİ (BACKLOG E1)

§3'ün kalan açık maddesi: auth'u geçmiş, tanımlı opcode'lu, iyi biçimli
girdinin saniye başına hacmi. Eskiden tek sınır odanın per-tick çekme
bütçesiydi (bağlantı başına 16/tick ≈ 30 Hz'de 480/sn): **odayı** korur,
göndericiyi değil — fazlası göndericinin kendi bounded kanalında birikir,
dolunca **kendi** girdisi düşer (`actions_dropped`, atfeli). Bir sayı
seçmek oynanış kararıdır; kullanıcı kararı (2026-09-27): **opt-in yapı
taşı** — bağlantı başına token bucket, varsayılan KAPALI, sayıyı
oyun/config verir, aşan girdi düşer ve sayılır.

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **Yer: bağlantı aktörü**, protokol kontrollerinden sonra, odanın kanalına `try_send`'den ÖNCE (`gsb-core/src/conn/gate.rs`, `conn/actor/input.rs`) | Aktör her frame'i görür, tek bağlantının durumunu paylaşmadan tutar ve girdinin odaya maliyet olduğu nokta orasıdır. Reddedilen frame hiç kuyruklanmaz, çekilmez, damgalanmaz, ingest edilmez. *Elenen:* odanın READ fazında sınırlamak — girdi zaten kanalda olurdu, tick onu çekip atmak için harcanırdı; per-tick bütçe "girenler arasında adalet"tir, "ne girer" değil |
| 2 | **Sayı ODANIN: `RoomConfig::input_rate`** (`InputRate { per_sec, burst }`, ikisi de sıfırdan büyük; `None` = kapalı). Registry join'de odanın config'inden damgalar ve yeni `Seat` cevabıyla (entity + aksiyon kanalı + hız) bağlantıya verir | Limit oynanış parametresi; bir sunucunun odaları farklı mod/hızda koşabilir (B18). Registry her odanın config'ini zaten tutar: tek damga iki oda biçimini (tek/sharded) ve iki join yolunu (taze/resume) kapsar; oda ve shard aktörleri hızı hiç görmez. *Elenen:* bağlantı/dinleyici düzeyinde sunucu geneli tek sayı — oda başına fark (lobi/maç) ifade edilemezdi |
| 3 | **Kova bağlantınındır, oda geçişinde yeniden AYARLANIR, dolmaz.** İlk sınırlı join kovayı dolu kurar; sonraki her join önceki hızla şimdiye kadar doldurur, yeni hızı koyar (her dolum güncel burst'e kırpar); sınırsız bir oda kapıyı kapatır ama seviyeyi unutmaz | Jeton zamanla birikir, oda atlayarak değil: bir leave/join döngüsü (sınırsız bir odadan geçerek bile) taze bir burst satın alamaz. Yavaş odada geçen süre o odanın hızıyla kazanır |
| 4 | **Aşan girdi düşer, kuyruklanmaz, sayılır — ihlal DEĞİL.** `ConnSample::input_rate_limited` → net raporu → `input_rate_limited=` satırı → aile tablosu (`gsb_net_input_rate_limited_total` Prometheus'ta, `gsb_net_input_rate_limited` OTLP'de) → loadgen teli (`GSME`). Bağlantı başına ilk düşürmede bir `warn` (bağlantı + eş adresi) | Aşım yapan istemci doğru bir istemcinin gönderemeyeceği hiçbir şey göndermiyor: sayı oyunun sıkı seçebileceği bir oynanış limiti; bütçeye yazmak onu bir kopmaya çevirirdi (§3.2'nin HEARTBEAT gerekçesinin aynısı: dürüst-ama-hızlı istemci, ör. 144 Hz girdi ya da lag sonrası boşalan tampon). Maliyet zaten sınırlı: reddedilen frame O(1), bounded inbox ve okuyucu pompası TCP'de göndericiyi kendi kendine yavaşlatır. Oyun kovmak isterse sayacı/uyarıyı okur |
| 5 | **Yalnız oyun girdisi ölçülür:** kayıtlı game-band opcode'ları. AUTH/JOIN/LEAVE/HEARTBEAT kontrol bandı; **RPC_REQ ölçülmez** | Bir RPC isteğine tam bir cevap borçlu (sessiz düşürme istemciyi kendi zaman aşımına bekletirdi); hacmi zaten sınırlı — bağlantı ve oda başına bekleyen istek cap'leri (aynı tick'te cevaplanır) ve odanın çekme bütçesi. Bağlantı tarafı bir RPC kapısı ayrı madde (BACKLOG D9) |
| 6 | **Sıra: protokol kontrolleri önce.** Tanımsız opcode hâlâ hard ihlal, odada değilken gelen girdi hâlâ race-class `NotInRoom` — kova boşken de | Kapı bir sınıflandırmayı yutmamalı: ihlal bütçesi ile hız sınırı farklı soruları cevaplar |
| 7 | **Saat: tick saati** (`gsb_core::ticker::now()`, TICK-ARCHITECTURE "Tick saati") | Hız "odanın saniyesi başına"dır; paused saatte sanal bir saniye bir saniyelik dolum demek — duvar saatinde mikrosaniye olurdu ve dürüst istemci reddedilirdi. Üretimde ikisi aynı an. Kapalıyken hiç saat okunmaz |
| 8 | **O(1), tahsis yok, zamanlayıcı görev yok:** seviye varışta, son varıştan geçen süreden hesaplanır; birim nano-jeton (`geçen_ns × per_sec`, `u128`, doyan aritmetik) | Kayan nokta ve yuvarlama sapması yok; aşırı değerler (u32::MAX hız, yıllarca boşta) taşmaz, burst'e doyar |
| 9 | **Yapılandırma:** oyunun varsayılanı `GameModule::input_rate()` (sağlanan metot, varsayılan `None`) → düz `input_rate_hz`/`input_burst` → `[rooms.<id>]`; `input_rate_hz = 0` kapalı (oyunun sayısının da üstünde), `input_burst` yazılmazsa bir saniyelik; bir tablo limitin tamamını yazar (bkz. OPS §2) | Sayı oyunun (kendi dürüst temposunu bilir), operatör ezer. Varsayılan KAPALI: anahtar yoksa ve oyun sayı vermiyorsa davranış bayt bayt bugünkü — kanal dolar, `actions_dropped` sayar |

**Kilitler:** `gsb-core/src/conn/gate/tests.rs` (kova aritmetiği, oda
geçişi, yeniden ayar), `gsb-core/tests/input_rate.rs` (paused saatte
canlı registry + oda: flooder tam olarak burst + dolum alır, odanın tick
başı çekişi burst'ü aşmaz, tam sınır hızında dürüst istemci dokunulmaz,
ihlal 0, bağlı kalır; sınırsız varsayılan bugünkü yolu izler),
`gsb-core/tests/input_rate/gate.rs` (RPC/heartbeat ölçülmez, protokol
kontrolleri maskelenmez, yeniden join doldurmaz), `gsb-server/tests/input_rate.rs`
(TCP üstünden config anahtarı; oyunun varsayılanı başlangıç + admin
odalarına aynı şablondan).

**Kalan yüzey:** bağlantıya atıflı ilk-beş listesi yok (yalnız `warn`);
kaynak adres başına sınır yok (bağlantı başına — D11 ile aynı eksen);
limit ihlali kovma/kapatma politikası oyunun.

## 3.5. Oturum yaşam döngüsü: iki saat, iki yön

Reader pump'un idle penceresi (`idle_timeout_secs`) yarım-açık TCP'yi
yakalar. Soketin DİĞER yarısı bağlantı sınırları turuna kadar sınırsızdı:
okumayı bırakan ama bağlantısını açık tutan bir istemcinin alım penceresi
kapanır, writer pump soket yazmasının içinde süresiz park eder, giden
kanal DOLU kalır (KAPALI değil — yani `w_closing` teardown'ı tetiklenmez),
oda her tick bir frame düşürür ve oturum hiçbir şey alamazken oda
slot'unu ve registry satırını tutmaya devam eder. Gelen sessizlik bunu
göremez, çünkü böyle bir istemcinin sessiz olması gerekmez (ROADMAP'teki
şekil tam olarak "okumayı bırakan ama göndermeye devam eden" istemcidir).

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **`write_stall_secs` (vars. 10 sn; `0` kapatır)** — soketi bu süre boyunca tek bir BAYT bile kabul etmemiş (ve yazılacak bir şeyi olan) bağlantı olağan teardown'la kapatılır | rUDP REL bandının canlılık sınırının TCP tarafındaki kardeşi; `idle_timeout_secs` ile aynı sözleşme (f64 saniye, `0` kapalı) — ikisi bir çifttir, yön başına bir saat |
| 2 | Ölçü **İLERLEME**, yaş değil — ve **BAYT**, kare değil | Yaş düşük-Hz odayı cezalandırır, yüksek-Hz'i ödüllendirir. İlerleme ise tam ayırmak istediğimiz iki istemciyi ayırır: geride kalan ama HÂLÂ BOŞALTAN istemcinin soketi bayt kabul eder ve saat her baytta yeniden başlar — pencereden uzun süren bir KARENİN ortasında da ("yavaş istemci tolere edilir" sözleşmesi aynen durur, düşen snapshot'ları eskisi gibi sayılır); hiç boşaltmayan istemcininki hiç bayt kabul etmez. **Düzeltme (stall gözlemlenebilirliği turu):** ilk uygulama saati yalnız bir karenin gönderimi BÜTÜN OLARAK tamamlanınca sıfırlıyordu; kare pencereden uzun sürede boşalınca okuyan istemci öldürülüyordu — kareler büyüdükçe ilerleme sınırı sessizce bir yaş sınırına dönüşüyordu (10k ölçümü: ~80 KB kareler, 4486 öldürme). Sinyal: yazıcıdaki monoton bayt sayacı (`gsb_net::pump::WriteProgress`), bekleyen işlemin kendi `&mut`'u üzerinden okunur; deadline'da baytlar ilerlemişse AYNI işlem yeni bir pencereyle beklenir |
| 3 | Saat writer pump'un yazma deadline'ında; bağlantı başına zamanlayıcı görev YOK | Reader'ın idiomunun aynısı: tek awaited işlemi bir deadline'a sarmak (`tokio::time::timeout` tek future'a; ikinci canlı kaynak değil) |
| 4 | Verdict aktörün mailbox'ından gider, soketten değil; ve `sink.close()` çağrılmaz. Writer pump doğumda (mailbox boşken) mailbox'ın kendi kapasitesinden **bir slot ayırır** (`try_reserve_owned`) ve hükmü giden kanalı kapatmadan ÖNCE o slota senkron postalar | Teardown, tıkanmış olan şeyin bir bayt daha kabul etmesini asla gerektirmemeli (rUDP `die()` ile aynı ilke; B66'dan beri `die()` aynı ayrılmış slotu da kullanır — `pump::verdict`). Ayrılmış slot: aşırı yükte mailbox tam hüküm anında DOLUdur (aktör tıkalı giden kanalda park etmiş, reader istemcinin karelerini arkasına diziyor); `try_send` düşer, kapanıştan sonraki awaited `send` ise aktör posta kutusuna bakıp çıktıktan sonra varır ve kapanış `outbound_dead` sayılırdı (10k: koşu başına 67–172). Bedel: bağlantı başına bir mailbox slotu; await yok, sınırsız kanal yok |
| 5 | rUDP bu saati almaz | Datagram `try_send_to` park etmez; o yönün canlılığı REL bandının ACK-ilerleme saatidir |
| 6 | Kalıntılar (BACKLOG B15, w1 turu): (a) ve (c) düzeltildi, (b) ölçülmüş bir ayar notu | **(a) TLS kuyruğu — düzeltildi.** rustls kendi ≤64 KiB şifreli metnini tutar; kare yazıcısı rustls'e verilen düz metni sayıyordu, karenin kuyruğu `poll_flush` içinde sayısız boşalıyordu: o kuyruğu pencere başına 64 KiB'tan yavaş okuyan (vars. 10 sn'de 6,5 KB/sn) eş okurken kesiliyordu. Artık TLS kapısı TCP akışını el sıkışmadan önce sayan bir adaptöre sarar (`gsb-net/src/wire.rs`, `Wire`): saat SOKETİN aldığı baytla işler, düz TCP'deki gibi. Kilit: `tls::tests::slow_reader` (önce 64 KiB'ta "write stall" ile kesildi; sağır eş hâlâ düşer). **(c) WS'de iki pencere — düzeltildi.** Soket yazıcı görevinin baytları pompayı uyandırmaz (kuyruk yuvası ancak bütün kare çıkınca boşalır); pompa onları ilk kez deadline'da görüp pencereyi O ANDAN yeniden başlatıyordu: hüküm son bayttan iki pencereye kadar sonra geliyordu (ölü eş `write_stall_secs`'in iki katı tutuluyordu). Görev artık her yazmanın ANINI da kaydeder (`WireCount`), pompa pencereyi bayttan başlatır (`WriteProgress::last_write_at`). Kilit: `pump::writer::tests` (duraklatılmış saat: son bayt 0,1 sn'de, pencere 10 sn → hüküm önce 20 sn'de, şimdi 10,1 sn'de), `ws::tests::slow_reader`. **(b) Çekirdek uyanma histerezisi — ayar notu, kapatıldı.** Dolu gönderim tamponunda bloklu yazıcıyı çekirdek ancak tamponun bir kısmı boşalınca uyandırır; ölçüldü (loopback, okuyan eş ~48 KB/sn): `sk_sndbuf` 32 KiB'ta uyanma başına ~12 KB / 0,26 sn, 128 KiB'ta ~30 KB / 0,64 sn, 512 KiB'ta ~124 KB / 2,6 sn — yani uyanma ≈ `sk_sndbuf`'un ¼'ü (0,23–0,38) boşalınca. Saat bu yüzden r < ~¼·B/W hızla okuyan istemciyi tıkalı sayar; böyle bir istemcinin yalnız çekirdek tamponundaki birikimi B/r > ~4W'dir (vars. ≥ 40 sn geride) — "geride ama boşaltıyor" sözleşmesinin pratik sınırı. WAN'da B, cwnd'ye göre büyür (uygulama sınırlı akışta onlarca KB; eşik birkaç KB/sn); loopback'te 64 KiB MSS B'yi MB'lara çıkarır (otomatik ayarla 2,6–4 MiB: 8 sn'lik ölçümde ikinci uyanma hiç gelmedi). Kapatma gerekçesi: etkilenen istemci oyun açısından ölüdür (her karesi ≥ 4W bayat), varsayılanı değiştiren bir çare (TCP_NOTSENT_LOWAT, küçük SO_SNDBUF) sağlıklı istemcinin tamponlamasını değiştirir. Tetik (BACKLOG w1 satırı): `write_stall_secs`'i birkaç saniyeye indirmek isteyen bir kurulum ya da okumaya devam ettiği gösterilen bir istemcinin `write_stall` ile kapatılması — o zaman çare deadline'da soketi tokio'nun hazırlık önbelleğini atlayarak bir kez denemek (çekirdek `sendmsg` histerezissiz kabul eder). Yan not: WS kapısının 64 yuvalı kuyruğu çekirdek tamponunun kardeşidir (saat yalnız kuyruk dolunca işler, TCP'de tampon dolunca işlediği gibi) — sınırlı, yanlış hüküm değil |

## 3.6. Sunucu-başlatımlı kapanışların sayımı (stall gözlemlenebilirliği turu)

Bir koruma sessizce kesiyorsa ölçüm yalan söyler: 10k ölçümünde sunucu
4486 oturumu write stall ile kapattı, istemci özeti `errors=0` dedi —
tıkanmış soket ERROR bildirimini taşıyamaz, istemci tarafında hiçbir
sayaç kıpırdamaz. Artık sunucunun KENDİ kararıyla bitirdiği her oturum
sebebiyle sayılır: `gsb_net_server_closes_total{reason=…}` (DESIGN §12).

| Sebep | Koruma |
|---|---|
| `idle_timeout` | §3.5 reader idle penceresi; rUDP demux idle süpürmesi |
| `write_stall` | §3.5 writer ilerleme saati |
| `rel_dead` | rUDP REL canlılık sınırı / retransmit tavanı |
| `violation_budget` | §3 ihlal bütçesi (AUTH seli dahil — §3.1) |
| `preauth_budget` | §3.3 pre-auth kare bütçesi |
| `stream_rejected` | taşıma seviyesi red: `max_frame_bytes`, çözülemeyen kare, WS protokol ihlali, bozuk TLS kaydı (önceden istemci kapanışı gibi görünüyordu). İstemciye artık en-iyi-çaba, beklemesiz ERROR 9 `stream rejected: …` gider; WS'te kapının kendi kapanış çerçevesi bildirimdir, bozuk TLS kaydında oturum ölü olduğundan bildirim inmez (DESIGN §5.6) |
| `conn_cap` / `unauth_cap` | `max_connections` / §4 unauthed cap'i, doğumda red |
| `unauth_source_cap` | §4.3.2 kaynak başına unauthed sınırı (`max_unauth_conns_per_source`, D12), doğumda red |
| `superseded` | aynı kimliğin yeni oturumu eskisini kapattı |
| `room_gone` | oda oturumun altında yok edildi / öldü |
| `outbound_dead` | giden kanal kapalı bulundu ve kayıtlı hüküm yok (çoğunlukla yazma hatasıyla gitmiş bir peer'ın kuyruğu; bekleyen bir stall hükmü ya da peer kapanışı mailbox'tan okunup ona atfedilir — stall hükmü dolu mailbox'ta da oradadır, §3.5 karar 4) |
| `idle_input` | odanın girdi-boşta tavanı (`max_idle_input_secs`) opt-in `afk_action = disconnect` altında: heartbeat atan ama oyun girdisi göndermeyen üye politikaya verildi, oda registry'den bağlantının kapatılmasını istedi (BACKLOG E6, RECONNECT §16.1). İstemciye en-iyi-çaba, beklemesiz ERROR 9 `input idle: …` gider |
| `kicked` | oyun mantığı üyeyi attı (`TickCtx::kick` / `gsb_kit::game::kick`, BACKLOG E8, RECONNECT §16.3): üyelik oyunun `on_disconnect` politikasıyla bitti, oda registry'den bağlantının kapatılmasını istedi. İstemciye en-iyi-çaba, beklemesiz ERROR 9 `kicked: <oyunun gerekçesi>` (≤ 256 bayt) gider |

Sayılmayanlar, bilerek: istemci-tarafı son (EOF, RST, WS kapanış el
sıkışması) — dökme değildir; sunucu kapanışı (`Shutdown`) — oturum
hakkında hüküm değil ve toplayıcı onunla birlikte öldüğü için
gözlenemez (istemci yine de bilgilendirilir: en-iyi-çaba ERROR 14,
DESIGN §5.6); ticket / protokol sürümü reddi — bağlantı açık kalır;
girdi-boşta tavanı varsayılan `afk_action = leave_room` ile — entity'yi
politikaya verir, üyeliği bitirir, oturumu bitirmez (`disconnect` ile
bitirir: `idle_input`). Ardından gelen oyun kareleri `ERROR 6` (race
sınıfı) alır — ihlal bütçesinin mevcut kuralı.

## 3.7. WebSocket kapısının RFC 6455 uyumu (WS uyum kapısı turu)

El yazımı RFC 6455 kapısı `[[listeners]]`'tan (`transport = "ws"`)
erişildiği için servis yolunda. Okuyucu (`gsb-net/src/ws/reader/`)
RFC 6455 §5 (çerçeveleme) ve §7'ye (kapanış) karşı kural kural
denetlendi. Her kural okuyucu seviyesinde bir testle kilitli:
`ws::tests::{fragmentation, framing, close_frames}` çıplak bir
`WsReader`'ı loopback soketten besler ve reddin kapanış kodunu doğrudan
giden kuyruktan okur (arada peer yok, kod birebir görünür). "Hata" =
bağlantı kapanış koduyla düşürülür, reader pump `StreamRejected` bildirir
(§3.6 `stream_rejected`).

| Kural | Önce | Şimdi | Test |
|---|---|---|---|
| §5.4 açık parçalı mesajın içinde yeni BIN, FIN'siz | yarım mesaj **sessizce atılıp** yenisine başlanıyordu | 1002 | `fragmentation::a_new_unfinished_binary_frame_inside_an_open_message_fails_with_1002` |
| §5.4 açık parçalı mesajın içinde yeni BIN, FIN'li | mesajın **içinde** oyun karesi olarak teslim ediliyordu | 1002 | `fragmentation::a_new_complete_binary_frame_inside_an_open_message_fails_with_1002` |
| §5.4 açık BIN mesajının içinde TEXT | 1003 | 1002 | `fragmentation::a_text_frame_inside_an_open_binary_message_fails_with_1002` |
| Açık mesaj yokken TEXT (sözleşme) | 1003 | 1003 | `fragmentation::a_text_frame_with_no_message_open_still_fails_with_1003`, `protocol::text_message_is_rejected_with_1003` |
| Açık mesaj yokken CONT | 1002 | 1002 | `fragmentation::a_continuation_with_no_message_open_fails_with_1002` |
| Parçalar arasında PING / PONG / CLOSE | izinli | izinli: pong cevaplanır, mesaj tek kare olarak birleşir; close el sıkışmayı tamamlar, yarım mesaj bırakılır | `fragmentation::ping_and_pong_between_fragments_leave_the_message_intact`, `fragmentation::a_close_between_fragments_completes_the_close_handshake` |
| §5.2 RSV1-3, uzantı yokken | 1002 | 1002 | `framing::reserved_bits_without_an_extension_fail_with_1002` |
| §5.2 ayrılmış opcode (3-7, 0xB-0xF) | 1002 | 1002 | `framing::reserved_opcodes_fail_with_1002` |
| §5.5 kontrol çerçevesi > 125 B ya da parçalı | 1002 | 1002 (125 B sınırı kabul) | `framing::oversized_or_fragmented_control_frames_fail_with_1002` |
| §5.1 maskesiz istemci çerçevesi | 1002 | 1002 | `framing::an_unmasked_client_frame_fails_with_1002`, `protocol::unmasked_client_frame_is_rejected_with_1002` |
| §5.2 64-bit uzunluğun MSB'si 1 | 1009 (tavan aşımı sanılıyordu) | 1002, tavandan önce | `framing::a_64_bit_length_with_the_high_bit_set_fails_with_1002` |
| §5.2 minimal olmayan uzunluk kodlaması (16/64-bit biçimde kısa uzunluk) | kabul | 1002 | `framing::non_minimal_length_encodings_fail_with_1002`; sınırlar: `framing::minimal_lengths_at_every_form_boundary_are_delivered` |
| Tavan üstü uzunluk | 1009 | 1009 | `framing::a_64_bit_length_over_the_ceiling_still_fails_with_1009`, `protocol::oversized_declared_length_is_rejected_with_1009` |
| §5.5.1 1 baytlık close yükü | 1002 | 1002 | `close_frames::a_one_byte_close_payload_fails_with_1002` |
| §7.4 gönderilemez kapanış kodu (0-999, 1004-1006, 1015, 1016-2999, ≥ 5000) | **yankılanıyordu** | 1002 | `close_frames::an_unsendable_close_code_fails_with_1002` |
| §7.4 gönderilebilir kod (1000-1003, 1007-1014, 3000-4999) | yankı | yankı (yalnız kod) | `close_frames::every_sendable_close_code_is_echoed` |
| §8.1 UTF-8 olmayan kapanış sebebi | kod yankılanıyordu | 1007 | `close_frames::a_close_reason_that_is_not_utf8_fails_with_1007`; 123 B çok-baytlı sebep kabul: `close_frames::a_maximal_utf8_reason_is_accepted` |
| §7.1 kapanış el sıkışması (kod yankısı, sonra sunucu önce kapatır) | var | var | `close_frames::an_empty_close_is_echoed_empty`, `protocol::close_handshake_echoes_code_and_reports_peer_closed` |
| §5.5.1 kapanış çerçevesinden sonra veri çerçevesi yok | fan-out'un hata kapanışı ile teardown arasındaki penceresi kapanışın ARKASINA yazılabiliyordu (B13'ün ERROR 9'u her seferinde yazılırdı) | soket yazıcı görevi — tel sırasını gören tek yer — kapanıştan sonraki veri çerçevesini atar | `after_close::no_data_frame_follows_a_close_frame`; uçtan uca `stream_rejected::a_websocket_violation_gets_the_close_frame_and_nothing_after_it` |

### Kararlar

| # | Karar | Gerekçe (elenen alternatif) |
|---|---|---|
| 1 | Açık BIN mesajının içindeki TEXT **1002**, 1003 değil | İlk kusur tipi değil çerçevelemedir: text destekleyen bir uç da orada 1002 göndermek zorunda (Autobahn 5.18 text-içinde-text için 1002 bekler). 1003'ü korumak (elenen) ihlali "desteklenmeyen veri" gibi gösterirdi. Tek başına TEXT 1003 kalır: sözleşme kararı değişmedi |
| 2 | Minimal olmayan uzunluk **reddedilir** (1002) | RFC bunu gönderene MUST olarak koyar. Tarayıcı asla üretmez; aynı çerçevenin iki kodlaması aradaki bir ayrıştırıcıyla anlaşmazlık vektörüdür. Kabul etmek (elenen) uyumlu hiçbir istemciye yaramazdı |
| 3 | 1012-1014 **gönderilebilir** sayılır | IANA kayıtlı (servis yeniden başlıyor / sonra dene / bad gateway). Yalnız RFC'nin 1000-1011'i (elenen) "yeniden başlıyorum" diyen bir peer'ı 1002 ile düşürürdü |
| 4 | Yankı yalnız kodu taşır, sebebi değil | RFC "genellikle kodu yankılar" der; sebebi doğrulamak yetiyor, geri göndermek bir şey kazandırmaz |

### Autobahn kapısı (CI `autobahn` işi)

- **Hedef** `gsb-net/examples/ws_autobahn.rs`: üretim kapısı (handshake,
  ayrıştırıcı, birleştirme, kontrol çerçeveleri, kapanış, pump'lar) +
  **opak mesaj eşlemesi** (`WsMessageMapping::Opaque`: binary mesaj
  olduğu gibi yankılanır). Autobahn bir echo sunucusu bekler ve rastgele
  binary yükleri geri ister; oyun zarfı bunları 1007 ile reddeder. Elenen
  alternatifler: (a) `gsb-server`'ı hedeflemek: echo vakalarının hemen
  hepsi sözleşme gereği FAIL olurdu, sinyal kalmazdı. (b) Harness'e ayrı
  bir okuyucu: test edilen kod üretim kodu olmazdı. (c) Opak modu bir cargo
  feature'ının arkasına koymak: varsayılan clippy/test örneği derlemezdi.
  Opak eşleme config'ten seçilemez; TEXT orada da 1003'tür.
- **Kapsam:** her vaka koşar, sözleşme gereği dışlananlar hariç. Text
  echo / text UTF-8 vakaları kapı text'i 1003 ile reddettiği için
  dışlanır: `1.1.*`, `3.2-3.4`, `4.1.3-5`, `4.2.3-5`, `5.3-5.8`, `5.15`,
  `5.18-5.20`, `6.*`, `7.1.1`, `7.1.5`, `7.1.6`, `9.1/9.3/9.5/9.7.*`,
  `10.*`. `12.*` ve `13.*` da dışlanır, çünkü permessage-deflate
  sunulmuyor. Her dışlamanın gerekçesi tek yerde:
  `.github/autobahn/autobahn.py` (`EXCLUDED`); spec'i o dosya üretir,
  raporu o dosya yargılar.
- **Build'i kırma kuralı:** koşan her vaka hem `behavior` hem
  `behaviorClose`'da OK/INFORMATIONAL olmalı. `ACCEPTED` (bugün boş; her
  giriş gerekçeli) dışında NON-STRICT / FAILED / UNCLEAN / WRONG CODE
  build'i kırar. Her gruptan bir zorunlu vaka koşmuş olmalı; dışlanan
  bir vaka koşmuşsa da kırılır.
- **Not:** Autobahn'ın parçalama grubu (5.*) binary mesaj
  araya girmesini hiç denemez (vakaların hepsi text). Bu turun kapattığı
  açık yalnız yukarıdaki birim testleriyle kilitli; Autobahn onu
  yakalamazdı.
- **Yerelde koşuldu (ebeveyn, 2026-09-25):** kullanıcı imaj indirmeye
  izin verdi; `crossbario/autobahn-testsuite:25.10.1` fuzzingclient
  modunda CI'nın adımlarıyla birebir koşuldu: **98 vaka koştu, 96 OK +
  2 INFORMATIONAL (davranış ve kapanış), `autobahn.py check` geçti**,
  `ACCEPTED` boş kaldı. Ajanın ilk yazdığı etiket `0.8.2` Docker Hub'da
  **yok** (`docker manifest inspect` ile doğrulandı) — CI işi ilk
  koşusunda imajı çekemeden düşerdi; etiket `25.10.1`'e sabitlendi.
  Ajan turunda (indirme izni yokken) ayrıca doğrulananlar:
  harness'e elle yazılmış bir istemciyle Autobahn biçimli 69 vaka
  (0 B-16 MiB binary echo, chop'lar, ping/pong, RSV, opcode, parçalı
  kontrol, 7.x kapanış kodları, 7.5.1'in baytları) yeşil geçti; CI
  adımlarının kabuk kısmı (harness başlatma, port bekleme, spec üretimi)
  yerelde koştu; `check` sentetik raporlarla sınandı; YAML parse edildi.
  İlk CI koşusu beklenmedik bir sonuç verirse iki yol var: düzelt, ya da
  `ACCEPTED`'e gerekçesiyle ekle. Sessizce gevşetmek yok.

## 4. Pre-auth tahsis sınırı (Tur B)

| # | Karar | Gerekçe |
|---|---|---|
| 1 | Registry, bağlantı tablosunda **auth durumu** tutar (`ConnOpened` unauthed açar; AUTH başarısı bildirilir) | Cap'in görüleceği tek yer registry'nin tablosudur (bağlantı sayısı orada yaşar — mevcut ilke) |
| 2 | **Unauthed cap:** `max_connections * %25` (min 64); aşan yeni bağlantı ERROR 9 ile nazikçe reddedilir | Script'li handshake fırtınasının (çok IP'li) bellek büyütmesini sınırlar; meşru yavaş-auth akışı için bol pay |
| 3 | Sayımlar aktör-local sayaçlarla; detach/resume bu sınıfa girmez (resume ticket'lıdır, authed sayılır) | RECONNECT semantiği korunur |

### 4.1 rUDP parçalama: yeniden birleştirme sınırları (rUDP parçalama turu)

rUDP oyun bandında bütçeyi aşan kare FRAG datagram'larına bölünür
(DESIGN §6 "MTU"). Yeniden birleştirme **yalnız istemcide** yapılır ve
her durumu sabitle sınırlıdır; sunucu parça kabul etmez.

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **Sunucu istemci → sunucu FRAG'ını reddeder** (demux `frag_refused` sayar, hiçbir şey iletmez, oturumun idle penceresi de tazelenmez) | Girdiler onlarca bayt — parçalamaya ihtiyaç yok. Sunucuda birleştirme durumu, handshake'i geçmiş her oturumun sunucuya tutturabileceği bellek olurdu ve bütün oturumların paylaştığı tek demux görevinde dururdu; reddetmek demux'u datagram başına durumsuz ve O(1) tutar. Tanımadığı tür zaten `bad_datagrams`'a düşüyordu; ayrı kol ve sayaç kararı görünür kılar |
| 2 | Mesaj başına en çok **16 parça** (`FRAG_MAX_COUNT`); başlıkta `count` 2..=16, `index < count` değilse parça reddedilir (`frag_rejected`); aynı mesajın parçaları `count`'ta anlaşmazsa reddedilir | Tek mesajın tutabileceği bellek parça sayısıyla sınırlı (≤ 16 × alım tamponu). Sunucu tarafında aynı tavan: 16 parçaya sığmayan kare eski yoldan atılır + sayılır |
| 3 | Aynı anda en çok **4 yarım mesaj** (`FRAG_SLOTS`, slot = id mod 4); slot'taki mesajdan daha yeni id eskisini düşürür (`frag_dropped_incomplete`), daha eski id'nin parçası reddedilir | Sabit dizi, map yok, büyüme yok: datagram başına O(1). Geç parça düşmüş mesajı diriltemez; id'ler seri sırayla karşılaştırıldığı için sarma (65535 → 0) yeni mesajı eski saymaz |
| 4 | Oturum başına **64 KiB** tutulan parça (`FRAG_MEM_CAP`); aşılacaksa önce en eski BAŞKA yarım mesaj düşer | Meşru tepe ~2 mesaj (grup karesi + private full) × ~10 KB; 64 KiB 3× pay. Slot sayısı × parça tavanı zaten sınırlar, bayt tavanı en kötü durumu (4 × 16 büyük parça) yarıya indirir |
| 5 | İlk parçasından **250 ms** sonra tamamlanmamış mesaj düşer (`FRAG_MAX_AGE`; sonraki parça geldiğinde süpürülür, 4 slot = sabit iş) | Parçalar tek yazıcıdan art arda çıkar; 250 ms her gerçekçi jitter'ın üstünde, bir keep-alive periyodunun altında |
| 6 | **Kontrol bandı parçalanmaz**; bütçeyi aşan kontrol karesi oturumu bitirir (`rel_dead`), seq harcanmadan | Güvenilir bant parça başına ACK gerektirirdi; aşan kontrol karesi zaten bir hatadır (değişken alanları istemcinin tek bütçe-içi datagram'ını yansıtır) — sessiz atmak eskiden akışı tıkıyordu |

**Saldırı yüzeyi.** FRAG, RAW kadar sahte üretilebilir (rUDP'de imza yok —
§6 NOT-DONE "rUDP crypto"): istemcinin portunu bilen ve sunucunun
adresini taklit eden biri parça enjekte edebilir. Sınırlar bunun
maliyetini istemci başına 64 KiB ve datagram başına sabit işle
tutar; meşru yarım mesajları düşürmek (snapshot bandını bozmak) sahte
RAW snapshot enjekte etmekten daha güçlü bir saldırı değildir.

### 4.2 rUDP el sıkışma kabulü: amplifikasyon ve sahte proof (H turu)

El sıkışma artık kayıpta kendini iyileştiriyor (DESIGN §6 "El sıkışma
kaybı"): sunucu doğrulanan proof'a 5 baytlık bir kabul (`ACK{1}`)
yollar, istemci onu görene dek proof'u yeniden gönderir. Stateless
cookie'nin iki özelliği korunuyor:

| # | Karar | Gerekçe |
|---|---|---|
| 1 | Kabul **yalnız doğrulanan proof'a** gider; sahte ya da süresi geçmiş proof'a (ve challenge isteğine) hiçbir şey dönmez | Kabul, dönüş yolunun sahibi olduğunu kanıtlamış adrese borçlanır: oran 5/18 < 1. Sahte kaynaklı trafik hâlâ en fazla aynı boyutta challenge alır (oran ≤ 1, değişmedi) |
| 2 | Kurulu oturumun adresinden gelen proof **yeniden doğrulanır**; geçerliyse oturumun güncel ACK'iyle cevaplanır, geçersizse ya da challenge isteğiyse cevapsız kalır | Kurulu oturum, adres taklidiyle o oturuma ACK yansıtmak için kullanılamaz. Yakalanan proof'un tekrarı en fazla 10-20 sn (cookie penceresi) boyunca yalnız o adrese 5 B'lik ACK üretir — yeni oturum değil |
| 3 | Yeniden gönderilen proof **ikinci oturum açmaz** (aynı adres = aynı oturum, tek `ConnectionId`), güvenilir durumu sıfırlamaz | Tekrar oynatılan proof mevcut oturumu bozamaz ya da çoğaltamaz; oturum tahsisi hâlâ proof başına en fazla bir |
| 4 | İstemcinin vazgeçme sınırı (5 sn) bir cookie diliminin (10 sn) altında — derleme zamanı `assert` | Yeniden gönderim ilk cookie'yi kullanır; son kopya da pencere içinde kalır, yeni challenge istemek (yeni cookie, yeni tahsis penceresi) gerekmez. Cookie son kullanma semantiği (yakalanan proof 10-20 sn'de ölür) değişmedi |

Kilit: `udp::demux::tests::handshake` (çift proof tek oturum + güncel
ACK; doğrulanmayan proof ve challenge isteği cevapsız; rotasyonu aşan
yeniden gönderim kurar, iki dilim eski kopya cevapsız),
`udp::tests::forged_proof_is_rejected` (sahte proof'a cevap yok),
`udp::tests::handshake` (kayıp proof/challenge/kabul iyileşir; vazgeçiş
temiz `TimedOut`, zombi yok).

### 4.3 El sıkışma sınırı: kapı başına, accept döngüsünün dışında (B31)

B29'un WS yük ölçümü (DESIGN §5.7) el sıkışan kapıların — WS
yükseltmesi, TLS, QUIC — el sıkışmayı `accept()`'in İÇİNDE yaptığını ve
sunucunun accept döngüsünün her accept'i sırayla beklediğini gösterdi:
yükseltme isteği göndermeyen TEK bir soket kapıyı el sıkışma süre
sınırı (10 sn) boyunca kilitliyordu — arkasındaki 20 istemcinin connect
p50'si 9610 ms; önemsiz bir hizmet reddi. Bağlanma fırtınasında backlog
taşıyor, başarısız el sıkışma döngüyü 100 ms geri çekiyordu. TLS kapısı
aynı yapıdaydı; QUIC'te el sıkışmayı quinn'in sürücüsü yürütse de
`accept` onu (ve istemcinin bi-stream'ini) bekliyordu — aynı seri yapı.

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **El sıkışma bağlantı başına kendi görevinde** (`gsb_net::transport::intake`): kapının kabul görevi ham bağlantıyı alır, ona bir yuva ve bir el sıkışma görevi verir; biten uç nokta bir kuyrukla `accept`'e gelir — `Listener::accept`'in şekli sunucu için değişmedi | Sessiz ya da yavaş istemci yalnız kendi yuvasını tutar, kapıyı değil; başarısız el sıkışma accept döngüsüne hiç ulaşmaz (hata yok, geri çekilme yok) |
| 2 | **Sınır: kapı başına uçuştaki el sıkışma sayısı = unauthed cap** (`max_unauth_conns`'un çözülmüş değeri, vars. 25 000; cap `0` ile kapatılmışsa türetilmiş varsayılan — `boot/start/pre_auth.rs`). Yeni bir config anahtarı yok; `gsb-net` taşıma yapılarında alan var (`max_pending_handshakes`, doğrudan gömene vars. 1024) | Uçuştaki el sıkışma, unauthed bağlantı olma yolundaki bağlantıdır: tek pre-auth bütçesi, iki evre. El sıkışma evresi sonrakinden ucuz tüketilemez (başkalarını kapıda reddettirmek için tutulması gereken soket sayısı, cap'i sessiz oturumlarla doldurmak için gerekenle aynı) ve sunucunun cap'le zaten tutabileceğinden fazlasını tutmaz. Cap'in kapatılması (dış kapı var) kapıyı sınırsız bırakmaz — el sıkışma her dış kapının görebileceği noktadan önce |
| 3 | **Sınırın üstünde ucuz ret, kuyruk yok:** WS/TLS'te soket el sıkışmasız kapanır; QUIC'te quinn `refuse` (CONNECTION_REFUSED, el sıkışma yok). Ret sayılır. Yuva ham accept'ten accept döngüsünün uç noktayı almasına dek tutulur; kuyruğun her girdisi bir yuva taşıdığından kuyruk uzunluğu ≤ sınır | Hiçbir yerde sınırsız bekleme yok; ret yolunda yazma (ör. HTTP 503) yok — yavaş okuyana yazmak iş olurdu |
| 4 | Her el sıkışma kendi süre sınırı altında: WS/TLS/QUIC 10 sn (değişmedi; QUIC'te bi-stream açılışı dahil) | Sessiz istemci bir yuvayı en çok bu kadar tutar |
| 5 | Kabul görevi, her el sıkışma görevi ve bekleyen `accept` dinleyicinin `Door`'u (B16) altında: `close()` — ya da son tutamacın düşmesi — ham accept'i, uçuştaki her el sıkışmayı (soketi düşer) ve kuyrukta bekleyeni keser | `stop()` hâlâ hemen biter, `StopReport` ağaç içi kapılarda 0 abort; kapanmış kapı yarım bağlantı tutmaz |
| 6 | **Sayaçlar:** `Listener::handshake_stats()` → `HandshakeStats { in_flight, completed, refused, timed_out, failed }` (el sıkışmayan kapılarda `None`); kabul görevi biterken özet `info` ("handshake intake stopped"); sınıra dayanan dönemin ilk reddi tek `warn` (dönem başına bir, ret başına değil); başarısız/süresi dolan her el sıkışma eskisi gibi `warn` | Dinleyiciler için metrik yolu yok (rUDP demux'ının sayaçları da kapanış özetiyle görünür); `handshake_stats` gömene ve testlere sayaçların kendisini verir. Ret başına uyarı, ret selinde log seli olurdu |

**Kalan yüzey.** Sınır kapı başınadır: varsayılanda tek bir kaynak
bütün yuvaları (vars. 25 000 soket, her biri ≤ 10 sn) hâlâ tutabilir —
fark, bunun artık TEK soket değil cap kadar soket gerektirmesi (unauthed
cap'i sessiz oturumlarla doldurmanın bedeliyle aynı). Kaynak adres
başına sınır opt-in olarak var (§4.3.1, D11).

Kilit: `gsb-net` `transport::intake::tests` (takılı el sıkışma biteni
tutmaz; sınır reddeder ve sayar; biten el sıkışma alınana dek yuvasını
tutar; süre sınırı keser ve sayar; başarısız el sıkışma sayılır,
sıraya girmez; `close` uçuştakileri keser ve accept'i bitirir, kuyruğu
boşaltır; son tutamacın düşmesi kapıyı kapatır),
`ws::tests::off_accept` (sessiz eş kapıyı tutmaz; başarısız yükseltme
accept hatası değil; `close` uçuştaki yükseltmeyi keser; beş sessiz eş
= beş uçuşta; sınır üstü bağlantı hemen kapanır ve sayılır, geri
verilen yuva sonrakine hizmet eder), `tls::tests::off_accept` (aynı
üçü, gerçek rustls istemcisiyle), `quic::tests::off_accept` (bi-stream
açmayan eş kapıyı tutmaz; sınır üstü bağlantı `refuse` ile reddedilir),
`tls::tests::wrong_ca_fails_the_handshake` ve QUIC eşi (başarısızlık
accept hatası değil, sayaç), `gsb-server/tests/handshake_door.rs`
(sunucu arkasında: sessiz eşler varken TLS el sıkışması ve WS
yükseltme + AUTH < 2 sn; `max_unauth_conns = 1` iki kapının da
sınırı), `boot::start::pre_auth::tests` (sınırın türetimi).

### 4.3.1 Kaynak adres başına el sıkışma sınırı (D11)

§4.3'ün sınırı kapı başınadır; tek kaynak kapının bütün yuvalarını
tutabilir. `max_handshakes_per_source` (OPS §2) bir kaynak adresin bir
kapıda aynı anda tutabileceği el sıkışma yuvasını sınırlar — WS, TLS ve
QUIC kapıları (düz anahtarlardan türeyen kapı ya da `[[listeners]]`'ın
her girdisi). **Varsayılan: yazılmaz = sınır yok** — kapılar bugünkü
gibi; varsayılan yüzey değişmedi.

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **Varsayılan kapalı** (`None`; `0` da kapalı — diğer cap'ler gibi) | Doğru sayı dağıtımın: aynı NAT adresinin arkasındaki oyuncular (LAN partisi, operatör NAT'ı — CGNAT) o adresi paylaşır; her test ve loadgen koşusu tek loopback adresinden bağlanır — varsayılan bir sınır onları kırardı |
| 2 | **Kaynak = IPv4 adresi; IPv6'da /64 öneki.** IPv4'e eşlenmiş IPv6 adresi (`::ffff:a.b.c.d`, çift yığınlı soketin IPv4 istemcisi) IPv4 adresi sayılır | /64, bir ağın tek aboneye verdiği en küçük blok; içinde ana makine adresi istediği gibi seçer (SLAAC, gizlilik adresleri) — /128 başına sayım hiç sınır olmazdı. Eşlenmiş adresler hepsi `::/64`'te: eşlenmeseydi bütün IPv4 istemcileri tek kaynak olurdu |
| 3 | **Sayılan: uçuştaki el sıkışma**, bağlantı değil. Yuva ham accept'ten accept döngüsünün uç noktayı almasına dek (§4.3 #3); el sıkışması biten bağlantı kaynağın sayısından düşer | Dürüst el sıkışma bir-iki gidiş-dönüş sürer; sınır yalnız eşzamanlı başlayanları kısar. Oturum sayısı ayrı sınırların işi (`max_connections`, `max_unauth_conns`) |
| 4 | **Sınır üstü ucuz ret, kuyruk yok:** WS/TLS'te soket el sıkışmasız kapanır (TLS/WS işi yok); sayaç `handshakes_refused_per_source`. Kapının kendi sınırının reddi (`handshakes_refused`) ayrı sayaçta — her ret anlamının adıyla tek yerde. Kaynak başına dönem başına tek `warn` (kaynağı adlandırır), ret başına `debug` | §4.3 #3'ün aynısı; ret seli log seli olmaz |
| 5 | **QUIC: kanıtlanmamış adres ayrı sayılır, sınırda Retry alır.** quinn'in `Incoming`'i adresini Retry jetonuyla kanıtlayana dek sahte kaynaklı olabilir; saldırgan kurbanın adresiyle sahte Initial'lar yollayıp kurbanın sayısını doldurabilirdi. Kanıtlanmamış kaynak aynı adresin kanıtlanmışından ayrı sayılır; sınırdaki kanıtlanmamış bağlantı reddedilmez, durumsuz Retry alır (yuva yok; `handshakes_retried_per_source`); gerçek sahibi yanıtlayıp kanıtlanmış döner, sahte kaynak dönemez. Kanıtlanmış ve hâlâ sınırda olan reddedilir (`refuse`) | Hedefli ret saldırısı kapanır; gerçek bir kaynak QUIC'te sınırı en çok iki kez tutar (bir kanıtsız, bir kanıtlı). Retry yalnız sınır yazılıp aşılınca: dürüst istemci için bir gidiş-dönüş, tel değişmez |
| 6 | **Kilitsiz:** tablo kapının kabul görevinde (yuvaları alan tek görev). Yuva başka yerde bırakılır (el sıkışma görevi, accept döngüsü, kapanışın boşaltması): `Drop`'u kaynağı kabul görevinin kuyruğuna yollar, görev her karardan önce boşaltır | B31'in yuvaları atomik; kaynak tablosu tek sahipli görev-yerel durum (`gsb_net::transport::intake` `source`) |
| 7 | **Sınırlı bellek:** tablo girdisi yalnız kaynak yuva tuttukça yaşar (son yuvayla gider); her yuva kapının sınırından biri — tabloda en çok kapı sınırı kadar girdi, bırakma kuyruğunda en çok o kadar anahtar. Sahte kaynaklı QUIC seli de tabloyu bu sınırın ötesine büyütemez | TCP kaynağı üç yollu el sıkışmadan sonra sahte olamaz; QUIC'inki olabilir — ama her girdi bir yuva |
| 8 | Düz TCP ve rUDP kapsam dışı | Düz TCP'nin el sıkışma evresi yok (bağlantı hemen pre-auth oturum); o evrenin kaynak başına sınırı registry'de — §4.3.2 (D12). rUDP el sıkışması durumsuz çerez, yuva tutmaz; registry'ye kaydedilmiş rUDP oturumları §4.3.2'nin sınırına girer, demux'ın kayıttan önceki oturum tablosunun kaynak başına sınırı rUDP sertleştirme turunun işi (BACKLOG B89) |

**Boyutlama.** Sınır, aynı adresin arkasından aynı gidiş-dönüş
penceresinde (pratikte aynı saniyede) bağlanan oyuncu sayısına payla
konur: ev ve küçük ofis için 8–16, LAN partisi ya da büyük CGNAT
havuzunun arkasındaki bölge için 32–64. Kötü niyetli tek kaynak en çok
bu kadar yuvayı (her biri ≤ 10 sn) tutar; kapının sınırı B'yi tüketmek
için B/sınır kadar adres (IPv6'da /64 öneki) gerekir. Dürüst bir
oyuncunun reddi `handshakes_refused_per_source`'ta görünür; ret saniyede
birkaçı aşıyorsa sınır büyütülür.

Kilit: `gsb-net` `transport::intake::source::tests` (kaynak tanımı:
IPv4, /64, eşlenmiş adres, kanıtsız ayrı; sınır yalnız kendi kaynağını
reddeder ve sayar; kanıtsız ayrı sayılır; bırakılan, alınan, süresi
dolan, başarısız yuva kaynağına döner; tablo kapı sınırını aşmaz ve
son yuvayla boşalır; sınırsız varsayılan), `tls::tests::per_source`
(gerçek kapı: sınırdaki kaynağın bağlantısı hemen kapanır ve sayılır,
başka kaynak — 127.0.0.2 — hizmet alır, bırakılan yuva kaynağa döner,
ret toplayıcıya ulaşır), `quic::tests::per_source` (kanıtsız sınırda
Retry, kanıtlanmış ikinci bağlanır, üçüncü reddedilir, başka kaynak
etkilenmez), `gsb-server/tests/handshakes_per_source.rs` (varsayılan
yok, ayrıştırma, `[rooms.<id>]` ve `[[listeners]]` reddeder; sunucu
arkasında TLS ve WS kapısında ret, başka kaynaktan TLS ve WS + AUTH,
retler rapora ulaşır).

### 4.3.2 Kaynak adres başına unauthed bağlantı sınırı (D12)

Düz TCP kapısının el sıkışma evresi yok: eş ilk bayttan unauthed
oturumdur ve §4.3.1'in sınırı onu hiç görmez — tek adres §4'ün bütün
unauthed havuzunu (vars. 25 000) doldurup başka her kaynağı
reddettirebiliyordu. Diğer kapıların oturumları da el sıkışmaları bitince
aynı havuza girer ve el sıkışmayı bitirmek ucuzdur.
`max_unauth_conns_per_source` (OPS §2) bir kaynağın havuzdan aynı anda
tutabileceği bağlantıyı sınırlar. **Varsayılan: yazılmaz = sınır yok.**

| # | Karar | Gerekçe |
|---|---|---|
| 1 | **Sınır registry'de, havuzun sayıldığı yerde; her kapının oturumu girer** (rUDP ve QUIC dahil — kayıt anında her kapının adresi kanıtlıdır: TCP/QUIC el sıkışması bitti, rUDP çerezi döndü) | Havuz tek yerde sayılıyor (§4 #1); kapıya göre ayırmak WS/TLS'i havuzu doldurmanın açık yolu bırakırdı. Adresi olmayan taşıma (`peer()` yok) kaynak başına sayılmaz |
| 2 | **D11'in anahtarı değil, kardeş anahtar** (`max_unauth_conns_per_source`) | Başka evre, başka kaynak: D11 uçuştaki el sıkışmayı (ms, kapı yuvası, yalnız WS/TLS/QUIC) sayar; bu, AUTH gidiş-dönüşü (ticket doğrulayıcısı dahil) boyunca yaşayan oturumu, her kapıda. Değerleri farklı olabilir; ikisi birlikte yazıldığında bir bağlantı iki evrede ayrı ayrı sınırlanır |
| 3 | **Çift sayım yok:** el sıkışma yuvası accept döngüsü uç noktayı alınca bırakılır, `ConnOpened` ondan sonra gider — bir bağlantı iki sınırda aynı anda sayılmaz. Her ret yalnız onu reddeden evrede, o evrenin adıyla sayılır (`handshakes_refused_per_source` / `server_closes{reason="unauth_source_cap"}`) | "Her kaybı say" — her ret anlamının adıyla tek sayaçta |
| 4 | **Kaynak kuralı D11'inki, kod paylaşılır:** `gsb_core::source::Source` (IPv4; IPv6 /64; eşlenmiş adres IPv4'ü). `gsb-net`'in `SourceKey`'i onu sarar (+ QUIC'in kanıtsız bayrağı) | Tek kural, iki evre; `gsb-net` zaten `gsb-core`'a bağlı — yeni bağımlılık yok |
| 5 | **Sayılan: registry satırlarından hâlâ unauthed ve aynı kaynaktan olanlar** — havuzun kendi taramasıyla tek geçişte. Ayrı tablo yok: geri verilecek bir şey yok, sızıntı mümkün değil (satır hangi çıkıştan giderse sayıdan da gider). Durum: satır başına bir `Source`; unauthed satırlar `max_unauth_conns` ile sınırlı | Sayaç tablosu her satır silme yolunda bırakma ister (registry'de yedi silme yeri var); kaçan biri kaynağı sonsuza dek reddettirirdi. Tarama O(bağlantı), açılış yolunda — havuzun taraması zaten öyle (kontrol düzlemi hızında olay) |
| 6 | **AUTH başarısı ya da kapanış yeri geri verir; başarısız AUTH vermez** | Başarısız AUTH oturumu açık ve unauthed bırakır (ERROR 10/13); havuzun kuralıyla aynı. Sel ihlal bütçesiyle kapanır, kapanış yeri verir |
| 7 | **Ret: havuzun reddiyle aynı yol** — ERROR 9 + kapanış, satır yok; yeni `ServerClose::UnauthSourceCap` (`unauth_source_cap`), WS'te 1013. Kaynak sınırı havuzdan önce bakılır | Hem kaynağı hem havuzu dolu doğum kaynağın fazlasıdır; havuz etiketini sel kirletmez. Dönem başına tek `warn` (sonraki kayıtla sıfırlanır), ret başına `debug` |
| 8 | **Kilitsiz, tek sahip:** registry aktörü (havuzun sahibi) | Bekleme yok, yeni await yok |

**Boyutlama.** El sıkışma sınırı gibi (aynı adresin arkasından aynı
anda bağlanan oyuncu + pay), ama oturum AUTH bitene dek tutar — ondan
küçük olmamalı. Kötü niyetli tek kaynak havuzdan en çok bu kadarını
tutar; havuzu (`U`) doldurmak `U`/sınır adres ister.

Kilit: `gsb-core` `tests/unauth_per_source.rs` (sınır üstü kendi
etiketiyle ret, başka adres/önek etkilenmez, eşlenmiş adres ve aynı /64
aynı kaynak, adressiz bağlantı sayılmaz; AUTH ve kapanış yer verir,
başarısız AUTH vermez; yazılmamış/`0` sınırsız; kaynak ve havuz
reddi ayrılır), `source::tests` (kural), `gsb-server/tests/unauth_per_source.rs`
(varsayılan yok, `[rooms.<id>]`/`[[listeners]]` reddi; düz TCP
kapısında 127.0.0.1 sınırda ERROR 9 + EOF, 127.0.0.2 hizmet alır, AUTH
ve kapanış yer verir, ERROR 13'lük başarısız AUTH vermez, üç ret
`server_closes{reason="unauth_source_cap"}`'te, başka sebep 0).

### 4.4 Dinleme kuyruğu: accept'ten önceki çekirdek sınırı (B84)

`listen_backlog` (OPS §2, DESIGN §6) gsb'nin hiçbir kabul kuralından
ÖNCE gelen tek sınırdır: çekirdeğin el sıkışmasını bitirip sürecin
henüz `accept` etmediği bağlantılar. Varsayılan 128 — tokio'nun kendi
bind'inin verdiği, anahtardan önce her kapının sahip olduğu kuyruk;
varsayılan yüzey değişmedi.

| # | Karar | Gerekçe |
|---|---|---|
| 1 | Değer büyütülebilir, sessizce kıstırılmaz: `1..=2147483647`, `0` başlatmayı durdurur; çekirdek `min(değer, somaxconn)` uygular | Sıfır "kuyruk yok" değil (Linux yine bir bağlantı kuyruklar), niyet belirsiz; `somaxconn`'u aşmak hata değil, çekirdeğin kendi tavanı |
| 2 | Büyük kuyruk yeni bir sınırsızlık açmaz | Kuyruk `somaxconn` ile (Linux vars. 4096) sınırlı; kuyruktaki bağlantı, istemci veri gönderirse alma arabelleği kadar çekirdek belleği tutabilir, ama kapılar kuyruğu hevesle boşaltır (TCP'nin accept döngüsü, WS/TLS'nin kabul görevi — §4.3) ve accept'ten sonra `max_unauth_conns` / `max_connections` / el sıkışma sınırı geçerli. Kuyruk sınırlar arasında geçici bir tampon |
| 3 | SYN seli kuyruğun işi değil | Linux'ta yarım açık istek kuyruğu da aynı değerle sınırlı; dolunca SYN cookie'leri devreye girer (`net.ipv4.tcp_syncookies`) — büyük değer cookie'lerin başlama noktasını öteler, onları kapatmaz |
| 4 | Operatör kuyruğu beklenen katılma patlamasına göre boyutlar, "en büyük"e göre değil | Taşan kuyruk DoS değil, bekleme: taşan istemcinin SYN'i düşer, istemci ~1 sn sonra yeniden dener (`TcpExtListenOverflows` sayar). Kuyruğu gereğinden büyük tutmak yalnız kötü niyetli bir patlamanın accept'e kadar bekleyebileceği bağlantı sayısını büyütür |

Kilit: `gsb-net` `listen::tests` (değer `listen(2)`'ye ulaşıyor — 1'lik
kuyruk birkaç bağlantı kuyruklar, varsayılan on altısını; `0` ve C
`int`'i aşan değer soket açılmadan reddedilir), `tls::tests` (TLS
kapısının alanı soket kurucusuna ulaşıyor), `gsb-server`
`boot::backlog_tests` (sunucunun her TCP tabanlı kapısı ve ops soketi
config değerini alıyor), `tests/listen_backlog.rs` (varsayılan, ayrıştırma,
aralık dışı değer hiçbir şey bağlanmadan başlatmayı durdurur).

### 4.5 UDP kapılarının soket arabellekleri (B4)

`udp_recv_buffer_bytes` / `udp_send_buffer_bytes` (OPS §2, DESIGN §6)
rUDP ve QUIC kapılarının TEK soketinin çekirdek arabellekleridir.
Varsayılan: yazılmaz, dokunulmaz — sistem varsayılanı; varsayılan yüzey
değişmedi.

| # | Karar | Gerekçe |
|---|---|---|
| 1 | Değer `4096..=2147483647`; dışı başlatmayı durdurur; çekirdek `rmem_max`/`wmem_max`'ta keser (hata değil, uyarı) | Kapı başına tek soket: bellek oturum sayısıyla değil kapı sayısıyla çarpılır — en fazla kapı başına `2 × rmem_max` (+ `2 × wmem_max`) çekirdek belleği. Tavan operatörün sysctl'ü; sunucu `SO_RCVBUFFORCE` (ayrıcalık) istemez |
| 2 | Büyük arabellek sahte kaynaklı seli büyütmez | Kuyruktaki datagram yalnız sıra bekler: demux her birini aynı sınırlarla işler (cookie, oturum tablosu, datagram bütçesi — §4.1); el sıkışma durumsuz kalır. Daha derin kuyruk, demux'ın geride kaldığı bir selde datagram'ların çekirdekte değil sırada beklemesi demek — gecikme artar, iş artmaz |
| 3 | Arabellek kaybın çözümü değil, eşiği | Dolan kuyruk yine düşürür (`RcvbufErrors`); el sıkışma ve REL bandı kaybı zaten iyileştirir (DESIGN §6 "El sıkışma kaybı"). Operatör kuyruğu beklenen patlamaya göre boyutlar |

Kilit: `gsb-net` `listen::udp::tests` (istek sokete ulaşıyor — Linux'ta
iki katı okunur, tavanda kesilir ve kesinti algılanır; ayarsız soket
varsayılanı alır; aralık dışı soket açılmadan reddedilir),
`udp::tests::buffers` ve `quic::tests::buffers` (iki kapının soketi
değeri alıyor, kapı yine hizmet ediyor), `gsb-server`
`boot::backlog_tests` (sunucunun iki UDP kapısı config değerini
kurucuya veriyor), `tests/udp_buffers.rs` (varsayılan, ayrıştırma,
aralık dışı değer hiçbir şey bağlanmadan başlatmayı durdurur).

## 4b. Oyuncu kimliği = karakter anahtarı (K4)

K4'ten beri (GAME-MODULE "K4 — oyuncu kimliği → ev shard'ı") bağlantının
doğrulanmış kimliği yalnız resume anahtarı değil, oyunun **karakter
anahtarıdır** da: sharded odanın join yönlendiricisi
(`registry::HomeShard`) ve join kancası (`GameLogic::on_join_as` → kit'in
`Game::spawn_player_as`) onu alır; MMO kayıtlı karakteri onunla bulur.

| Yol | Kimlik | Güven |
|---|---|---|
| Ticket hook yapılandırılmış | `ValidatedTicket.player` (doğrulayıcının döndürdüğü; `Auth.name` yok sayılır) | Platformun kimliği. Hangi hesabın hangi karakteri oynayabileceği platformun kararı — bilete kodlanır (`player` = hesap/karakter); sunucu ayrıca bir karakter alanı kabul ETMEZ |
| Ticket'sız (eski yerel auth) | İstemcinin iddia ettiği `Auth.name` | **YOK — yalnız geliştirme / demo / loadgen yolu.** Herkes her karakter olarak girebilir, başkasının park edilmiş karakterini devralabilir (resume'da zaten öyleydi, RECONNECT §4) ve aynı adla canlı oturumu düşürebilir ("en son kazanan", ERROR 9). Üretimde ticket hook'u zorunlu |
| Boş kimlik | anonim | Kayıtlı karakter yok (MMO: varsayılan durak taşı), resume yok |

Yeni auth mekanizması eklenmedi; kimlik AUTH'ta zaten geçiyordu (wire
değişmedi). Kilit: `gsb-core/tests/join_identity.rs` (ticket yolunda
iddia edilen ad yönlendiriciye/kancaya ulaşmıyor),
`gsb-server/tests/mmo_home.rs::the_ticket_player_picks_the_character_not_the_claimed_name`
(`bob` diyen istemci ann'in biletiyle ann'in karakterini alıyor).

## 5. Test planı

Tur A: TLS ile tüm guardrail e2e'leri (plaintext parametresiyle parametrik);
self-signed handshake; yanlış CA reddi; tek-taraf-config başlatma hatası;
handshake-timeout kapanması.
Tur B: auth flood → bütçe tükenimi + kapanma; pre-auth heartbeat
fırtınası → sessiz sayaç; 64-frame aşımı → ERROR 9; unauthed cap
dolunca yeni bağlantının reddi + authed olanın etkilenmemesi.
Bağlantı sınırları turu: post-auth heartbeat fırtınası → aralık başına
tek cevap + sessiz sayaç + puanlanmama (`security.rs`), auth sınırının
tek sıfırlaması (`security.rs`), kısma ile idle penceresinin dikişi
(`e2e.rs`); hiç okumayan peer → writer pump'un stall raporu ve soketi
beklemeden çıkışı (`gsb-net` `tcp::tests::stall`), ve uçtan uca oturumun
bitişi + registry satırının bırakılışı (`write_stall.rs`).
Stall gözlemlenebilirliği turu: pencereden uzun süren tek bir kareyi
yavaş ama sürekli okuyan peer HAYATTA kalır — TCP (küçültülmüş çekirdek
tamponları), QUIC (istemcinin 2 KiB alım penceresi) ve WS yazma yolu
(`*::tests::slow_reader`); hiç okumayan WS peer'ı hâlâ ölür; her kapanış
yolu kendi sebep kovasında sayılır ve komşu kovalar kıpırdamaz
(`server_closes.rs`, `violation/closes.rs`, `write_stall.rs`).
WS uyum kapısı turu: §3.7 tablosunun her satırı okuyucu seviyesinde
(`gsb-net` `ws::tests::{fragmentation, framing, close_frames}`), opak
eşleme `ws::tests::opaque`'ta; CI'da Autobahn fuzzing client'ı.
B31: el sıkışma sınırının her kuralı — eşzamanlılık, sınır + ret,
süre sınırı, `close`'un uçuştakileri kesmesi, yavaşın hızlıyı
tutmaması — önce kırmızı, tek tek mutasyonla (§4.3 "Kilit").

## 6. NOT-DONE

- mTLS (istemci sertifikası) — ticket-auth yeterli v1'de
- TLS 0-RTT/session resumption ayarları — varsayılanlar
- Admin HTTP auth/TLS — OPS.md NOT-DONE devam
- Ops HTTP: auth/TLS yok (yukarıda). Kaynak sınırları var — başlık
  okuması (B47), eşzamanlı bağlantı tavanı ve yanıt yazmanın süre sınırı
  (B49, `http_max_connections` / `http_write_timeout_secs`), registry
  cevabını bekleyen yönlendirmenin süre sınırı (B90,
  `http_route_timeout_secs`, aşılırsa `504`; OPS §3); kaynak adres başına
  tavan yok (localhost sözleşmesi)
- rUDP crypto — deneysel statü
- Kaynak adres başına sınırın kapsamadığı evre: rUDP demux'ının kayıttan
  önceki oturum tablosu (§4.3.1 #8, BACKLOG B89; kaydedilmiş rUDP
  oturumları §4.3.2'ye girer)
- Post-auth girdi hacmi için bağlantıya atıflı ilk-beş listesi ve kaynak
  adres başına sınır (§3.4 "Kalan yüzey"; hız sınırının kendisi opt-in
  olarak var)
