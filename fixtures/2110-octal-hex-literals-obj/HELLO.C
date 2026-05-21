int main(void) {
  int hex = 0xABCD;
  int oct = 0177;
  int dec = 1000;
  return (hex & 0xF) + (oct & 0x7) + (dec & 0xF);
}
