# Third-party notices

## fan455_piano_synthesis — the piano modal model

`slooper/synths/piano.rs` is a real-time translation of the string +
hammer *modal synthesis* model from:

> **fan455_piano_synthesis** — an open-source piano sound synthesis
> project using physical modelling.
> Copyright (c) 2024 fan455
> https://github.com/fan455/fan455_piano_synthesis
> Working paper: arXiv:2409.03481

The bundled piano keeps that project's core note-making algorithm — a
sum of exponentially-decaying modal partials whose damping grows with
mode number, 1/n spectral amplitudes, alternating initial phases for a
hammer-struck string, and the hammer-force attack transient — adapted to
an allocation-free fixed-voice real-time voice and extended with standard
stiff-string inharmonicity and damper-pedal behaviour.

Licensed under the MIT License:

> Permission is hereby granted, free of charge, to any person obtaining a
> copy of this software and associated documentation files (the
> "Software"), to deal in the Software without restriction, including
> without limitation the rights to use, copy, modify, merge, publish,
> distribute, sublicense, and/or sell copies of the Software, and to
> permit persons to whom the Software is furnished to do so, subject to
> the following conditions:
>
> The above copyright notice and this permission notice shall be included
> in all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
> OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
> MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
> IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
> CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
> TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
> SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
