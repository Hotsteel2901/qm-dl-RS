# QQ 音乐「超级会员」接口逆向记录

抓包时间：2026-11-07
工具：Reqable（TUN 模式） + Reqable MCP（capture_live / capture_record_filter / capture_record_get_by_id）
会话：Firefox 已登录超级会员（uin 已脱敏），设备为本机

本文记录的是**实测结果**，不是猜测。所有「匿名走不通」的结论后面都跟着可复现的请求与返回码。

---

## 0. 采到了什么

本机 Firefox 的 Cookie 库仍是旧 schema（moz_cookies.value 明文），可以直接从 profile 里取到登录票据：

| Cookie | 作用 |
| --- | --- |
| qm_keyst / qqmusic_key | 音乐站登录票据，g_tk = hash33(qqmusic_key) |
| wxuin | 账号 uin（后续请求里的 uin） |

播放器实际使用的两个端点：

| 端点 | 方法 | 形态 |
| --- | --- | --- |
| https://u6.y.qq.com/cgi-bin/musics.fcg?sign=zzc...&encoding=ag-1 | POST | Content-Type: text/plain，body 是 AES-128-GCM 密文 |
| https://u.y.qq.com/cgi-bin/musicu.fcg | POST | 明文 JSON（同一批 module 的兼容入口） |

ag-1 请求体结构（已由 tools/decrypt_ag1.js 验证）：

    AES-128-GCM
    key  = base64("vTBfEND/dLbvVNq4NbXhzw==")   // 16 字节
    body = IV(12) + ciphertext + Tag(16)

sign 为 zzc 前缀的动态串；实测 musics.fcg 对 sign 做校验
（随手编一个纯数字 sign 会直接返回 {"code":2000}）。

---

## 1. 会员会话里的取流请求

明文入口（抓包里可读）：

    POST https://u.y.qq.com/cgi-bin/musicu.fcg
    Referer: https://y.qq.com/
    Origin:  https://y.qq.com
    Content-Type: application/json
    Cookie: qm_keyst=...; qqmusic_key=...; wxuin=...

    {
      "comm": {
        "cv": 4747474, "ct": 24, "format": "json",
        "platform": "yqq.json", "needNewCode": 1,
        "uin": "<uin>", "g_tk": <g_tk>, "g_tk_new_20200303": <g_tk>
      },
      "req_0": {
        "module": "vkey.GetVkeyServer",
        "method": "CgiGetVkey",
        "param": {
          "guid": "<10 位随机>",
          "songmid": ["<songmid>"],
          "songtype": [0],
          "uin": "<uin>",
          "loginflag": 1,
          "platform": "20",
          "filename": ["<前缀><media_mid>.<扩展名>"]
        }
      }
    }

返回（会员）：

    {
      "code": 0,
      "req_0": {
        "code": 0,
        "data": {
          "sip": ["http://aqqmusic.tc.qq.com/", "http://sjy6.stream.qqmusic.qq.com/"],
          "midurlinfo": [{
            "songmid": "...",
            "filename": "M800002ug3qO0YirZV.mp3",
            "purl": "M800002ug3qO0YirZV.mp3?guid=...&vkey=...&uin=<uin>&fromtag=...",
            "result": 0,
            "vkey": "...",
            "vip_downfromtag": 0
          }],
          "expiration": 7200
        }
      }
    }

最终直链 = sip[0] + purl。

### filename 前缀表（media_mid 拼在后面）

| 前缀 | 内容 | 说明 |
| --- | --- | --- |
| M500 | 128 kbps MP3 | |
| M800 | 320 kbps MP3 | |
| F000 | FLAC 无损 | |
| C400 | 128 kbps M4A | |
| A000 | APE | 冷门 |
| RS02 | 60 秒试听 MP3 | **匿名唯一能拿到 purl 的前缀** |

---

## 2. 最关键的发现：直链是 bearer token

拿会员账号签出来的 sip+purl，**不带任何 Cookie、换 UA、裸 curl** 直接下：

    GET http://aqqmusic.tc.qq.com/M800...mp3?guid=...&vkey=...
    （无 Cookie）
    → HTTP 200, Content-Type: audio/mpeg

也就是说：

- 账号只出现在**签发**环节（vkey 服务端按 Cookie 判权限）；
- 一旦签发，URL 本身就是凭证，2 小时内**任何人**都能下载；
- 反向理解：想匿名拿到会员曲，就必须找到「能签发的人/服务」，绕不过这一层。

tools 里的票据桥（serve / --bridge）就是把这套性质产品化。

---

## 3. 为什么「纯匿名」签不出整曲（逐条实测）

对《恋曲1990》（songmid 000Jd2XU1i8Dpj，media_mid 002ug3qO0YirZV，317s）：

| 尝试 | 结果 |
| --- | --- |
| filename 16 种前缀（M500/M800/F000/A000/O400/O600/C400/C600/H000/D000/Q000/T000/S000/R300/RA00） | 全部 result=104003、purl="" |
| 仅 RS02（试听） | result=0，purl 有值（60 秒） |
| platform = 20 / 11 / 14 / yqq.json / wq | 全部 104003 |
| comm.ct = 11 | 101404（参数不合法） |
| songtype = 0/1 → 104003；2/4/8 → 101404 | 无解 |
| 伪造 uin（含真实会员 uin + 正确 g_tk，不带 Cookie） | 104003 |
| 旧接口 c.y.qq.com/base/fcgi-bin/fcg_music_express_mobile3.fcg | {"code":104003,"errinfo":"merror"} |
| CDN 直连（dl/isure/ws.stream.qqmusic.qq.com/M800...mp3，无 vkey） | 403 |
| 拿试听 RS02 的 vkey 去换 M800/M500/F000/C400 | 403（vkey 与路径绑定） |
| musics.fcg 随手构造 sign | {"code":2000} |

结论很硬：**权限判定只在 vkey 服务端的 Cookie 上，客户端没有任何可伪造的开关。**
网上那些「QQ 音乐匿名 320k 破解接口」，要么背后是别人的会员账号池，
要么返回的其实是 RS02 试听。

---

## 4. 三条真正可用的通道

| 通道 | 匿名？ | 音质 | 原理 |
| --- | --- | --- | --- |
| --cookie 本机账号 | 否 | 320k / FLAC | 用你自己的会员票据签发 |
| serve + --bridge 票据桥 | **下载端匿名** | 320k / FLAC | 一台机器持号签发，其它机器只消费 bearer URL |
| --mirror migu/kuwo/netease | **完全匿名** | 128k MP3 | 同一首歌在其它平台对匿名开放的免费流 |

匿名镜像的要点：

- **咪咕**：app.c.nf.migu.cn/MIGUM2.0/.../listen.do，toneFlag=PQ 匿名可得 128k 整曲；
  HQ/SQ 返回 999999「请升级客户端」（即需要登录/会员）。
- **酷我**：search.kuwo.cn/r.s → antiserver.kuwo.cn/anti.s，128k 整曲。
- **网易云**：music.163.com/song/media/outer/url，仅 fee=0/8 的曲目可用。

因为跨平台会撞上「翻唱 / 伴奏 / Live / DJ 版」，工具做了两道闸：

1. 版本标记硬过滤（原曲没有 伴奏/live/翻唱/cover/dj/... 时，带这些标记的候选直接淘汰）；
2. 下载后**本地解析时长**（自己写的 MP3/FLAC/M4A 帧解析，不依赖 ffprobe），
   与 QQ 的 interval 相差 >2.5s 就删文件报错。
   60 秒试听伪装成整曲、翻唱时长不匹配，都会在这步被拦下。

---

## 5. 扫码登录链路（QQ / 微信）

这一节是 2026-11 从 y.qq.com 线上页面里扒出来的，不是猜的：
https://y.qq.com/portal/wx_redirect.html 的内联 JS 就是官方换票逻辑，
里面直接写死了两个 appid 和两个 module。

### QQ

    GET  https://ssl.ptlogin2.qq.com/ptqrshow
         appid=716027609 & daid=383 & pt_3rd_aid=100497308     → PNG + qrsig
    轮询 GET  https://ssl.ptlogin2.qq.com/ptqrlogin
         ptqrtoken = hash33(qrsig, 种子 0)                      → ptuiCB('66/67/65/0', ...)
    扫码成功 → ptuiCB 第 3 个参数是 check_sig 地址，取出 ptsigx / uin
    GET  https://ssl.ptlogin2.graph.qq.com/check_sig?ptsigx=...&uin=...  → p_skey 等 Cookie
    POST https://graph.qq.com/oauth2.0/authorize（表单）
         g_tk = hash33(p_skey, 种子 5381)                       → 302 Location 里带 code
    POST https://u.y.qq.com/cgi-bin/musicu.fcg
         {"comm":{"tmeAppID":"qqmusic","tmeLoginType":"2"},
          "req":{"module":"QQConnectLogin.LoginServer","method":"QQLogin",
                 "param":{"code":"..."}}}                        → musickey / musicid

### 微信

    GET  https://open.weixin.qq.com/connect/qrconnect
         appid=wx48db31d50e334801
         redirect_uri=https://y.qq.com/portal/wx_redirect.html?login_type=2&surl=https://y.qq.com/
         scope=snsapi_login                                     → 页面里 /connect/qrcode/<uuid>
    GET  https://open.weixin.qq.com/connect/qrcode/<uuid>       → 470x470 JPEG
    轮询 GET  https://long.open.weixin.qq.com/connect/l/qrconnect?uuid=...&_=...
         window.wx_errcode=408 等待 / 404 已扫 / 405 已确认（带 wx_code）
    POST https://u.y.qq.com/cgi-bin/musicu.fcg
         {"comm":{"tmeAppID":"qqmusic","tmeLoginType":"1"},
          "req":{"module":"music.login.LoginServer","method":"Login",
                 "param":{"strAppid":"wx48db31d50e334801","code":"..."}}}

票据既可能通过 Set-Cookie（qm_keyst / qqmusic_key / uin / wxuin）下发，
也可能只出现在 JSON 的 musickey / musicid 里，两条都兜住。

### QQ音乐 App（手机客户端扫码）

    POST musicu.fcg   music.login.LoginServer / CreateQRCode     Content-Type: application/json
         comm  (Android) ct=11 cv=13020508 v=13020508 tmeAppID=qqmusic
               format=json inCharset/outCharset=utf-8 uid=3931641530
               QIMEI36=<设备指纹，失败时可用固定回退值>
         param {"tmeAppID":"qqmusic","ct":11,"cv":13020508}
         body  {"music.login.LoginServer.CreateQRCode":
                  {"module":"music.login.LoginServer","method":"CreateQRCode","param":{...}}}
         → data.qrcode = data:image/png;base64,... , data.qrcodeID , data.expiresIn

    MQTT 5.0 over WebSocket：wss://mu.y.qq.com:443/ws/handshake
         CONNECT
           AuthenticationMethod = "pass"
           UserProperty: tmeAppID=qqmusic, business=management,
                         hashTag=<qrcodeID>, clientTag=management.user,
                         userID=<qrcodeID>
         SUBSCRIBE management.qrcode_login/<qrcodeID>
           UserProperty: authorization=tmelogin, pubsub=unicast
         PUBLISH（状态在 UserProperty 的 type 字段）：
           scanned / canceled / timeout / loginFailed / cookies
         cookies 事件：
           {"cookies":{"qqmusic_uin":{"value":"..."},"qqmusic_key":{"value":"..."}}}

    POST musicu.fcg   music.login.LoginServer / Login            Content-Type: application/json
         comm  与 CreateQRCode 相同的 Android 公参 + tmeLoginType="6"
         body  {"music.login.LoginServer.Login":
                  {"module":"music.login.LoginServer","method":"Login",
                   "param":{"musicid":<qqmusic_uin>,"qrCodeID":"...",
                            "token":"<qqmusic_key>"}}}
         → data.musickey / data.musicid（同样以 JSON 为准，不依赖 Set-Cookie）

    ⚠ 换票必须用 Android 公参（ct=11 / cv=13020508）和 application/json；
      用 Web 端 comm（platform=yqq、cv=0）+ 表单 Content-Type 会被判为
      登录态失效，返回 code=1000。回包 key 是 `music.login.LoginServer.Login`。

连接阶段服务端可能回 `ServerMoved`（0x9D）并带 `ServerReference`；客户端需要重连到
`/ws/handshake/<ServerReference>`。本项目用 rumqttc 做 MQTT 层，遇到该状态会自动重试，
通常 1～2 次后即落到可服务的节点。

### 四个坑（都是真踩出来的）

1. **ptqrtoken 的 hash33 种子是 0**，不是 g_tk 那个 5381。
   种子搞错时服务端不给 ptuiCB，而是直接 WAF 403、响应体为空 —— 看起来像被墙，其实是算错。
2. **ptlogin2 前面挂着 TAPISIX WAF，按 TLS 指纹拦客户端**。
   同一个 URL，curl 200、reqwest（rustls）403、reqwest（native-tls）照样 403。
   登录链路因此走 curl 子进程，并顺手用它的 Cookie 罐。
3. **二维码不止一种格式**：QQ 给 1-bit 灰度 PNG（111×111），微信给 JPEG（470×470）。
   按扩展名判断会挂，要按 magic bytes；解码用 rqrr，终端重绘用 qrcode。
4. **终端二维码必须半块渲染 + 留静默区**：两个模块行拼一行（█▀▄），
   四周留 2 个模块白边，否则手机对不上焦。

## 6. 复现命令

    # 1) 看会员会话到底签了什么（只打印直链，不下载）
    qqmusic-dl download 000Jd2XU1i8Dpj -q flac --dry-run

    # 2) 纯匿名：QQ 拒绝 → 咪咕整曲
    qqmusic-dl --anon download 000Jd2XU1i8Dpj -o ~/Music

    # 3) 票据桥：A 机持号，B 机零账号
    A$ qqmusic-dl serve -p 8080
    B$ qqmusic-dl --anon download 000Jd2XU1i8Dpj --bridge http://A:8080 -o ~/Music

    # 4) 解密网页版 ag-1 请求体（配合 Reqable 导出）
    node tools/decrypt_ag1.js body.bin

    # 5) 扫码登录（Cookie 自动落盘）
    qqmusic-dl login wx
    qqmusic-dl login qq
