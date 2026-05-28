void poke(unsigned, unsigned, int);
int main(void) {
  poke(0xB800, 0, 0x0F41);
  return 0;
}
