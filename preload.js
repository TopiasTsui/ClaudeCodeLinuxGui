'use strict';

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('api', {
  pickDir: () => ipcRenderer.invoke('pick-dir'),
  sendMessage: (payload) => ipcRenderer.invoke('send-message', payload),
});
