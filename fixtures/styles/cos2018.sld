<?xml version="1.0" encoding="UTF-8"?>
<!-- COS2018 land-cover SLD (DGT). Generated: 83 level-4 classes on COS18_n4_C.
     Colours from the official DGT QML (github.com/PedroVenancio/cos_2018_dgt_symbology);
     class labels from the GeoPackage COS18_n4_L. Keep in sync with cos2018.mvt-style.json. -->
<StyledLayerDescriptor version="1.0.0"
    xmlns="http://www.opengis.net/sld" xmlns:ogc="http://www.opengis.net/ogc">
  <NamedLayer><Name>cos2018</Name><UserStyle>
    <Title>COS2018 - Carta de Uso e Ocupacao do Solo (DGT)</Title>
    <FeatureTypeStyle>
    <Rule>
      <Name>1.1.1.1</Name>
      <Title>1.1.1.1 - Áreas edificadas residenciais contínuas predominantemente verticais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6004d</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.1.1.2</Name>
      <Title>1.1.1.2 - Áreas edificadas residenciais contínuas predominantemente horizontais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.1.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e60014</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.1.2.1</Name>
      <Title>1.1.2.1 - Áreas edificadas residenciais descontínuas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.1.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ff0000</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.1.2.2</Name>
      <Title>1.1.2.2 - Áreas edificadas residenciais descontínuas esparsas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.1.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ff0032</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.1.3.1</Name>
      <Title>1.1.3.1 - 1.1.3.1</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.1.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ff0050</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.1.3.2</Name>
      <Title>1.1.3.2 - 1.1.3.2</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.1.3.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ff006e</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.2.1.1</Name>
      <Title>1.2.1.1 - Indústria e logística</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.2.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4df2</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.2.2.1</Name>
      <Title>1.2.2.1 - Instalações agrícolas e pecuárias</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.2.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4dd2</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.2.3.1</Name>
      <Title>1.2.3.1 - 1.2.3.1</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.2.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4da0</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.3.1.1</Name>
      <Title>1.3.1.1 - Equipamentos culturais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.3.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4d78</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.3.1.2</Name>
      <Title>1.3.1.2 - 1.3.1.2</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.3.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4d5a</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.3.2.1</Name>
      <Title>1.3.2.1 - Equipamentos desportivos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.3.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4d46</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.3.2.2</Name>
      <Title>1.3.2.2 - Equipamentos de lazer</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.3.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc4d28</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.1.1</Name>
      <Title>1.4.1.1 - Infraestruturas de produção de energia hídrica</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cc0000</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.1.2</Name>
      <Title>1.4.1.2 - Infraestruturas de produção de energia solar</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#9a0000</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.2.1</Name>
      <Title>1.4.2.1 - Infraestruturas de produção de energia de fonte fóssil</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6cccc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.2.2</Name>
      <Title>1.4.2.2 - 1.4.2.2</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6b4cc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.2.3</Name>
      <Title>1.4.2.3 - 1.4.2.3</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.2.3</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e696cc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.3.1</Name>
      <Title>1.4.3.1 - Subestações e postos de transformação de energia</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6cce6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.4.3.2</Name>
      <Title>1.4.3.2 - 1.4.3.2</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.4.3.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#d2cce6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.5.1.1</Name>
      <Title>1.5.1.1 - Rede rodoviária</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.5.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a600cc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.5.1.2</Name>
      <Title>1.5.1.2 - Rede ferroviária</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.5.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#6e00cc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.5.2.1</Name>
      <Title>1.5.2.1 - Terminais portuários de mar e de rio</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.5.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a64d00</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.5.2.2</Name>
      <Title>1.5.2.2 - Estaleiros navais e docas secas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.5.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a67400</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.5.3.1</Name>
      <Title>1.5.3.1 - Aeroportos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.5.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ff4dff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.1.1</Name>
      <Title>1.6.1.1 - Minas a céu aberto</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffe6ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.1.2</Name>
      <Title>1.6.1.2 - Pedreiras</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffd2ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.2.1</Name>
      <Title>1.6.2.1 - 1.6.2.1</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffb4ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.2.2</Name>
      <Title>1.6.2.2 - 1.6.2.2</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ff8cff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.3.1</Name>
      <Title>1.6.3.1 - 1.6.3.1</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e664ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.4.1</Name>
      <Title>1.6.4.1 - 1.6.4.1</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.4.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#be64ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.6.5.1</Name>
      <Title>1.6.5.1 - 1.6.5.1</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.6.5.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a064ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>1.7.1.1</Name>
      <Title>1.7.1.1 - Vazios sem construção</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>1.7.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffa6ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.1.1.1</Name>
      <Title>2.1.1.1 - Culturas temporárias de sequeiro e regadio</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffff00</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.1.1.2</Name>
      <Title>2.1.1.2 - Arrozais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.1.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6e600</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.2.1.1</Name>
      <Title>2.2.1.1 - Vinhas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.2.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e68000</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.2.2.1</Name>
      <Title>2.2.2.1 - Pomares</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.2.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#f2a64d</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.2.3.1</Name>
      <Title>2.2.3.1 - Olivais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.2.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6a600</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.3.1.1</Name>
      <Title>2.3.1.1 - Culturas temporárias e/ou pastagens melhoradas associadas a vinha</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.3.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffe6a6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.3.1.2</Name>
      <Title>2.3.1.2 - Culturas temporárias e/ou pastagens melhoradas associadas a pomar</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.3.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#fff0a6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.3.1.3</Name>
      <Title>2.3.1.3 - Culturas temporárias e/ou pastagens melhoradas associadas a olival</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.3.1.3</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#fffaa6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.3.2.1</Name>
      <Title>2.3.2.1 - Mosaicos culturais e parcelares complexos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.3.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffe64d</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.3.3.1</Name>
      <Title>2.3.3.1 - Agricultura com espaços naturais e seminaturais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.3.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6cc4d</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>2.4.1.1</Name>
      <Title>2.4.1.1 - Agricultura e viveiros protegidos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>2.4.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6cc82</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>3.1.1.1</Name>
      <Title>3.1.1.1 - Pastagens melhoradas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>3.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ffff4d</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>3.1.2.1</Name>
      <Title>3.1.2.1 - Pastagens espontâneas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>3.1.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ccf24d</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.1</Name>
      <Title>4.1.1.1 - Superfícies agrossilvícolas de sobreiro</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#f2cca6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.2</Name>
      <Title>4.1.1.2 - Superfícies agrossilvícolas de azinheira</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#dccca6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.3</Name>
      <Title>4.1.1.3 - Superfícies agrossilvícolas de outros carvalhos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.3</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#c8cca6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.4</Name>
      <Title>4.1.1.4 - Superfícies agrossilvícolas de outras folhosas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.4</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#b4cca6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.5</Name>
      <Title>4.1.1.5 - 4.1.1.5</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.5</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a0d7a6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.6</Name>
      <Title>4.1.1.6 - 4.1.1.6</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.6</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a0e6a6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>4.1.1.7</Name>
      <Title>4.1.1.7 - 4.1.1.7</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>4.1.1.7</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a0ffa6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.1</Name>
      <Title>5.1.1.1 - Florestas de sobreiro</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80ff00</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.2</Name>
      <Title>5.1.1.2 - Florestas de azinheira</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80eb00</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.3</Name>
      <Title>5.1.1.3 - Florestas de outros carvalhos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.3</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80d700</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.4</Name>
      <Title>5.1.1.4 - Florestas de castanheiro</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.4</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80c800</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.5</Name>
      <Title>5.1.1.5 - Florestas de alfarrobeira</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.5</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80b400</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.6</Name>
      <Title>5.1.1.6 - Florestas de eucalipto</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.6</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80a000</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.1.7</Name>
      <Title>5.1.1.7 - Florestas de acácias</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.1.7</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#808c00</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.2.1</Name>
      <Title>5.1.2.1 - Florestas de pinheiro bravo</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#00a600</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.2.2</Name>
      <Title>5.1.2.2 - Florestas de pinheiro manso</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#008c00</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>5.1.2.3</Name>
      <Title>5.1.2.3 - Florestas de outras resinosas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>5.1.2.3</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#007800</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>6.1.1.1</Name>
      <Title>6.1.1.1 - Matos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>6.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a6ff80</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>7.1.1.1</Name>
      <Title>7.1.1.1 - Praias, dunas e areais interiores</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>7.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6e6e6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>7.1.1.2</Name>
      <Title>7.1.1.2 - Praias, dunas e areais costeiros</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>7.1.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6e6d7</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>7.1.2.1</Name>
      <Title>7.1.2.1 - Espaços rochosos</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>7.1.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#cccccc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>7.1.3.1</Name>
      <Title>7.1.3.1 - Vegetação esparsa</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>7.1.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ccffcc</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>8.1.1.1</Name>
      <Title>8.1.1.1 - Pauis e turfeiras</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>8.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a6a6ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>8.1.2.1</Name>
      <Title>8.1.2.1 - Sapais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>8.1.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#ccccff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>8.1.2.2</Name>
      <Title>8.1.2.2 - Zonas entremarés</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>8.1.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a6a6e6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.1.1</Name>
      <Title>9.1.1.1 - Cursos de água naturais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#00ccf2</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.1.2</Name>
      <Title>9.1.1.2 - Cursos de água modificados ou artificializados</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.1.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#00f2f2</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.2.1</Name>
      <Title>9.1.2.1 - Lagos e lagoas interiores artificiais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80f2e6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.2.2</Name>
      <Title>9.1.2.2 - Lagos e lagoas interiores naturais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.2.2</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80dee6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.2.3</Name>
      <Title>9.1.2.3 - Albufeiras de barragens</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.2.3</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80c8e6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.2.4</Name>
      <Title>9.1.2.4 - Albufeiras de represas ou de açudes</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.2.4</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80b4e6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.1.2.5</Name>
      <Title>9.1.2.5 - Charcas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.1.2.5</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#80a0e6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.2.1.1</Name>
      <Title>9.2.1.1 - Aquicultura</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.2.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6e6ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.3.1.1</Name>
      <Title>9.3.1.1 - Salinas</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.3.1.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6ffff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.3.2.1</Name>
      <Title>9.3.2.1 - Lagoas costeiras</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.3.2.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#00ffa6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.3.3.1</Name>
      <Title>9.3.3.1 - Desembocaduras fluviais</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.3.3.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#a6ffe6</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    <Rule>
      <Name>9.3.4.1</Name>
      <Title>9.3.4.1 - Oceano</Title>
      <ogc:Filter><ogc:PropertyIsEqualTo>
        <ogc:PropertyName>COS18_n4_C</ogc:PropertyName><ogc:Literal>9.3.4.1</ogc:Literal>
      </ogc:PropertyIsEqualTo></ogc:Filter>
      <PolygonSymbolizer><Fill>
        <CssParameter name="fill">#e6f2ff</CssParameter>
      </Fill></PolygonSymbolizer>
    </Rule>
    </FeatureTypeStyle>
  </UserStyle></NamedLayer>
</StyledLayerDescriptor>
