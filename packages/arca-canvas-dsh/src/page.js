import { mountCanvas } from './render.js';
import './canvas.css';

const root = document.getElementById('arca-root');
mountCanvas(root, { pollMs: Number(root.dataset.pollMs) || 5000 });
