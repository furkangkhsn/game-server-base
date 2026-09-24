# gsb: Güvenlik Turu Tasarımı — TLS, Rate-Limit, Pre-Auth Sınırlar

> Durum: TASARIM (iki uygulama turu bu dokümanı sözleşme alır: A=TLS,
> B=sınırlar). Dış inceleme ailesinin kalan teknik maddeleri burada
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
| WS kapısının RFC 6455 uyumu (parça arası veri çerçevesi, uzunluk kodlaması, kapanış kodları) + CI'da Autobahn kapısı | WS uyum kapısı turu | ✅ Uygulandı (§3.7); Autobahn işi yerelde koşulmadı |
| rUDP cookie rotasyonu (yakalanan proof'un son kullanma tarihi) | rUDP doğruluk turu | ✅ Uygulandı (DESIGN §5, "Cookie rotasyonu"; slot = 10 sn, pencere 10-20 sn) |
| rUDP şifreleme/congestion | Kapsam DIŞI — rUDP deneysel statüde; kanıtlanmış taşıma ya da ayrı tur |
| Admin HTTP auth | OPS.md NOT-DONE (localhost sözleşmesi) |

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
  aynı aile
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

**Bütçeye yazılmıyor** (pre-auth gerekçesinin aynısı): kısma zaten
maliyeti sınırlıyor, dolayısıyla puanlamak düşman tarafında hiçbir şey
kazandırmaz; yalnız dürüst-ama-hatalı istemciyi (gevşek bir NAT
keepalive'ı) zorla düşürür.

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
| 4 | Verdict aktörün mailbox'ından gider, soketten değil; ve `sink.close()` çağrılmaz. Writer pump doğumda (mailbox boşken) mailbox'ın kendi kapasitesinden **bir slot ayırır** (`try_reserve_owned`) ve hükmü giden kanalı kapatmadan ÖNCE o slota senkron postalar | Teardown, tıkanmış olan şeyin bir bayt daha kabul etmesini asla gerektirmemeli (rUDP `die()` ile aynı ilke). Ayrılmış slot: aşırı yükte mailbox tam hüküm anında DOLUdur (aktör tıkalı giden kanalda park etmiş, reader istemcinin karelerini arkasına diziyor); `try_send` düşer, kapanıştan sonraki awaited `send` ise aktör posta kutusuna bakıp çıktıktan sonra varır ve kapanış `outbound_dead` sayılırdı (10k: koşu başına 67–172). Bedel: bağlantı başına bir mailbox slotu; await yok, sınırsız kanal yok |
| 5 | rUDP bu saati almaz | Datagram `try_send_to` park etmez; o yönün canlılığı REL bandının ACK-ilerleme saatidir |
| 6 | Bilinen kalıntılar | (a) TLS: kare tamponu boşaldıktan sonra rustls'in tuttuğu son ≤64 KiB `poll_flush` içinde bayt sayısı raporlamadan boşalır. (b) Çekirdek: dolu gönderim tamponunda bloklu yazıcı ancak tamponun ~üçte biri boşalınca uyandırılır — B baytlık tamponda B / (3 × pencere)'den yavaş okuyan istemci uyanmalar arasında sessiz görünür. (c) WS kapısında soketi başka görev yazar; bayt sayısı pump'a ilk kez deadline'da görünür, orada sınır son bayttan itibaren iki pencereye kadardır |

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
| `stream_rejected` | taşıma seviyesi red: `max_frame_bytes`, çözülemeyen kare, WS protokol ihlali, bozuk TLS kaydı (önceden istemci kapanışı gibi görünüyordu) |
| `conn_cap` / `unauth_cap` | `max_connections` / §4 unauthed cap'i, doğumda red |
| `superseded` | aynı kimliğin yeni oturumu eskisini kapattı |
| `room_gone` | oda oturumun altında yok edildi / öldü |
| `outbound_dead` | giden kanal kapalı bulundu ve kayıtlı hüküm yok (çoğunlukla yazma hatasıyla gitmiş bir peer'ın kuyruğu; bekleyen bir stall hükmü ya da peer kapanışı mailbox'tan okunup ona atfedilir — stall hükmü dolu mailbox'ta da oradadır, §3.5 karar 4) |

Sayılmayanlar, bilerek: istemci-tarafı son (EOF, RST, WS kapanış el
sıkışması) — dökme değildir; sunucu kapanışı (`Shutdown`) — oturum
hakkında hüküm değil ve toplayıcı onunla birlikte öldüğü için
gözlenemez; ticket / protokol sürümü reddi — bağlantı açık kalır;
girdi-boşta tavanı — entity'yi politikaya verir, oturumu bitirmez.

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

## 6. NOT-DONE

- mTLS (istemci sertifikası) — ticket-auth yeterli v1'de
- TLS 0-RTT/session resumption ayarları — varsayılanlar
- Admin HTTP auth/TLS — OPS.md NOT-DONE devam
- rUDP crypto — deneysel statü
