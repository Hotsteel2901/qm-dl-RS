#!/usr/bin/env node
/*
 * 解密 QQ 音乐网页版 musics.fcg 的 `encoding=ag-1` 请求体。
 *
 * 已验证的算法（2026-11 抓包）：
 *   AES-128-GCM
 *   key   = base64("vTBfEND/dLbvVNq4NbXhzw==")   // 16 字节
 *   结构  = IV(12 字节) + 密文 + Tag(16 字节)
 *
 * 用法：
 *   node tools/decrypt_ag1.js body.base64.txt
 *   node tools/decrypt_ag1.js body.bin
 *
 * 说明：该 key/结构对「请求体」验证通过；响应体使用另一套密钥，
 * 本项目下载器不需要它（改用明文 musicu.fcg 接口）。
 */
const fs = require('fs');
const crypto = require('crypto');

const KEY = Buffer.from('vTBfEND/dLbvVNq4NbXhzw==', 'base64');

function asRawBuffer(input) {
  const text = input.toString('utf8').replace(/\s+/g, '');
  if (text.length >= 32 && text.length % 4 === 0 && /^[A-Za-z0-9+/]+={0,2}$/.test(text)) {
    return Buffer.from(text, 'base64');
  }
  return input;
}

function main() {
  const file = process.argv[2];
  if (!file) {
    console.error('用法: node tools/decrypt_ag1.js <抓包请求体文件>');
    process.exit(2);
  }

  const raw = asRawBuffer(fs.readFileSync(file));
  if (raw.length < 12 + 16) {
    console.error(`数据太短（${raw.length} 字节），不是 ag-1 密文`);
    process.exit(1);
  }

  const iv = raw.subarray(0, 12);
  const tag = raw.subarray(raw.length - 16);
  const ciphertext = raw.subarray(12, raw.length - 16);

  try {
    const decipher = crypto.createDecipheriv('aes-128-gcm', KEY, iv);
    decipher.setAuthTag(tag);
    const plain = Buffer.concat([decipher.update(ciphertext), decipher.final()]);
    process.stdout.write(plain);
    if (process.stdout.isTTY) process.stdout.write('\n');
  } catch (e) {
    console.error(`解密失败（${e.message}）——可能不是 ag-1 请求体，或格式已变化`);
    process.exit(1);
  }
}

main();
