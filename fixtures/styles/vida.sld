<?xml version="1.0" encoding="UTF-8"?>
<!-- VIDA Google-Microsoft-OSM Open Buildings (Iberia) coloured by `bf_source` — the provenance
     of each deduped footprint, which is the whole point of the VIDA corpus.
     Data: https://source.coop/vida/google-microsoft-osm-open-buildings — ODbL v1.0.
     Fill-only (no Stroke): at city zoom footprints are a few px, so an outline would swamp the
     fill; it also keeps the raster cheap. Distinct hues, not a ramp — these are categories. -->
<StyledLayerDescriptor version="1.0.0"
    xmlns="http://www.opengis.net/sld"
    xmlns:ogc="http://www.opengis.net/ogc"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <NamedLayer>
    <Name>vida</Name>
    <UserStyle>
      <Title>VIDA Open Buildings — by source</Title>
      <FeatureTypeStyle>
        <Rule>
          <Name>microsoft</Name>
          <Title>Microsoft GlobalML</Title>
          <ogc:Filter><ogc:PropertyIsEqualTo>
            <ogc:PropertyName>bf_source</ogc:PropertyName><ogc:Literal>microsoft</ogc:Literal>
          </ogc:PropertyIsEqualTo></ogc:Filter>
          <PolygonSymbolizer><Fill>
            <CssParameter name="fill">#00d5ff</CssParameter>
          </Fill></PolygonSymbolizer>
        </Rule>
        <Rule>
          <Name>google</Name>
          <Title>Google Open Buildings v3</Title>
          <ogc:Filter><ogc:PropertyIsEqualTo>
            <ogc:PropertyName>bf_source</ogc:PropertyName><ogc:Literal>google</ogc:Literal>
          </ogc:PropertyIsEqualTo></ogc:Filter>
          <PolygonSymbolizer><Fill>
            <CssParameter name="fill">#39ff14</CssParameter>
          </Fill></PolygonSymbolizer>
        </Rule>
        <Rule>
          <Name>osm</Name>
          <Title>OpenStreetMap</Title>
          <ogc:Filter><ogc:PropertyIsEqualTo>
            <ogc:PropertyName>bf_source</ogc:PropertyName><ogc:Literal>osm</ogc:Literal>
          </ogc:PropertyIsEqualTo></ogc:Filter>
          <PolygonSymbolizer><Fill>
            <CssParameter name="fill">#ff1744</CssParameter>
          </Fill></PolygonSymbolizer>
        </Rule>
        <!-- else-filter: any future/unknown source still draws rather than punching a hole
             (the COS2023 "black holes" lesson: an unmatched feature renders transparent). -->
        <Rule>
          <Name>other</Name>
          <Title>other / unknown source</Title>
          <ElseFilter/>
          <PolygonSymbolizer><Fill>
            <CssParameter name="fill">#b0bec5</CssParameter>
          </Fill></PolygonSymbolizer>
        </Rule>
      </FeatureTypeStyle>
    </UserStyle>
  </NamedLayer>
</StyledLayerDescriptor>
