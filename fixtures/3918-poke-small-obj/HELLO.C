void poke(unsigned, unsigned, int);
int main(void) {
  poke(0x0040, 0x0017, 0);
  return 0;
}
