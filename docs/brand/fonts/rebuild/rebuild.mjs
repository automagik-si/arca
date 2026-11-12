// Rebuilds ../fredoka-latin.woff2 from the Google Fonts source file and fails unless the result is
// that file, byte for byte. CI runs it (the "Brand font rebuild" job); ../README.md has the call.
// Usage: node rebuild.mjs <path to Fredoka[wdth,wght].ttf> [where to write the rebuilt .woff2]
import subsetFont from 'subset-font'
import { readFileSync, writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'

const SOURCE_SHA256 = '2ba02e68b152868aef9ba28e24b3648c7d457fe6f25c761f2c2c53fb61a73fc8'
const TEXT_SHA256 = 'c7224eaa525b085e4152125d5ad531bff915b4e24015089e7e598032cc09a8b3'
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex')
const fail = message => { console.error(`font rebuild: ${message}`); process.exit(1) }

const [source, output] = process.argv.slice(2)
if (!source) fail('usage: node rebuild.mjs <Fredoka[wdth,wght].ttf> [output.woff2]')
const input = readFileSync(source)
if (sha256(input) !== SOURCE_SHA256) fail(`the source file is not the recorded one: sha256 ${sha256(input)}, expected ${SOURCE_SHA256}`)
// The characters kept, as one UTF-8 string with no newline at the end.
const text = readFileSync(new URL('text.txt', import.meta.url))
if (sha256(text) !== TEXT_SHA256) fail(`text.txt changed: sha256 ${sha256(text)}, expected ${TEXT_SHA256}`)

const rebuilt = Buffer.from(await subsetFont(input, text.toString('utf8'), {
  targetFormat: 'woff2',
  variationAxes: { wdth: 100, wght: { min: 300, max: 700 } },
}))
if (output) writeFileSync(output, rebuilt)
const committed = readFileSync(new URL('../fredoka-latin.woff2', import.meta.url))
console.log(`rebuilt:   ${rebuilt.length} bytes, sha256 ${sha256(rebuilt)}`)
console.log(`committed: ${committed.length} bytes, sha256 ${sha256(committed)}`)
if (!rebuilt.equals(committed)) fail('the rebuilt font is not the committed docs/brand/fonts/fredoka-latin.woff2')
console.log('font rebuild: the committed font is the rebuild of its source, byte for byte')
