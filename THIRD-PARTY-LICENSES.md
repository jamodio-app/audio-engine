# Licences des composants tiers — Jamodio Audio Engine

Jamodio embarque des composants open-source. Leurs licences (MIT / Apache-2.0)
imposent d'inclure le copyright et le texte de licence : les voici. La fonction
d'**isolation de voix (talkback)** est fournie par les deux composants suivants,
exécutés **100 % en local sur votre machine** (aucune donnée envoyée à un serveur).

> NB : cette page de crédits sera aussi accessible depuis l'agent (« À propos →
> Licences ») et le site web. Modèles exécutés via `tract` (moteur ONNX pur Rust).

---

## DeepFilterNet (isolation de voix — débruitage)

- Projet : https://github.com/Rikorose/DeepFilterNet
- Copyright © Hendrik Schröter et contributeurs DeepFilterNet.
- Licence : **au choix, MIT OU Apache-2.0**. Poids du modèle inclus sous la même
  licence. Entraîné sur le corpus DNS-Challenge (données de licences permissives).

## Silero VAD (isolation de voix — détection de parole)

- Projet : https://github.com/snakers4/silero-vad
- Copyright © Silero Team.
- Licence : **MIT**.

---

## Texte de la licence MIT

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Le texte complet de la licence Apache-2.0 (option pour DeepFilterNet) est
disponible sur https://www.apache.org/licenses/LICENSE-2.0.
