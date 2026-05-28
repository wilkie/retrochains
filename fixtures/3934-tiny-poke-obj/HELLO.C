void poke(unsigned, unsigned, int);
int main(void) {
  poke(0x0040, 0x0010, 1);
  return 0;
}
