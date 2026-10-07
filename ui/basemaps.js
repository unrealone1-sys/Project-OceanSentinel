'use strict';

/* Basemaps shared by both domains: OceanSentinel (ships) and Project Icarus
   (aircraft). Loaded before app.js / icarus.js, which read window.OS_BASEMAPS.

   All of these are keyless raster tile services, so a fresh install has a
   usable map with no accounts anywhere. Note the "dark" choice: CARTO's dark
   tiles now answer unauthenticated browsers with "API KEY REQUIRED" placeholder
   images, so this uses Esri's Dark Gray Canvas instead, which stays clean. */

window.OS_BASEMAPS = {
  satellite: {
    label: 'SATELLITE',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Imagery © Esri, Maxar, Earthstar Geographics',
    paint: { 'raster-opacity': 0.9, 'raster-saturation': -0.1, 'raster-brightness-max': 0.9 },
  },
  dark: {
    label: 'DARK',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Dark_Gray_Base/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Esri Dark Gray Canvas',
    paint: { 'raster-opacity': 0.95 },
  },
  streets: {
    label: 'STREETS',
    tiles: ['https://tile.openstreetmap.org/{z}/{x}/{y}.png'],
    tileSize: 256,
    attribution: '© OpenStreetMap contributors',
    paint: { 'raster-opacity': 0.85 },
  },
  ocean: {
    label: 'OCEAN',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/Ocean/World_Ocean_Base/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Esri Ocean Basemap',
    paint: { 'raster-opacity': 0.92 },
  },
  topo: {
    label: 'TOPO',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/World_Topo_Map/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Esri World Topo Map',
    paint: { 'raster-opacity': 0.9 },
  },
  natgeo: {
    label: 'NAT GEO',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/NatGeo_World_Map/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Esri National Geographic',
    paint: { 'raster-opacity': 0.9 },
  },
  relief: {
    label: 'RELIEF',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/World_Shaded_Relief/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Esri Shaded Relief',
    paint: { 'raster-opacity': 0.9 },
  },
};
