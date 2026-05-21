int main(void) {
  unsigned long a = 0x12345678UL;
  unsigned long r = a >> 1;
  return (int)(r >> 16);
}
