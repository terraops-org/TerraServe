<StyledLayerDescriptor version="1.0.0" xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
 <NamedLayer><Name>airports</Name><UserStyle><FeatureTypeStyle>
  <Rule>
   <Name>major</Name>
   <MaxScaleDenominator>20000000</MaxScaleDenominator>
   <PointSymbolizer><Graphic><Mark><WellKnownName>circle</WellKnownName>
     <Fill><CssParameter name="fill">#1e1e1e</CssParameter></Fill></Mark><Size>6</Size></Graphic></PointSymbolizer>
   <TextSymbolizer>
     <Label><ogc:PropertyName>name</ogc:PropertyName></Label>
     <Font><CssParameter name="font-size">16</CssParameter></Font>
     <Halo><Radius>2</Radius><Fill><CssParameter name="fill">#ffffff</CssParameter></Fill></Halo>
     <Fill><CssParameter name="fill">#141414</CssParameter></Fill>
   </TextSymbolizer>
  </Rule>
 </FeatureTypeStyle></UserStyle></NamedLayer>
</StyledLayerDescriptor>
