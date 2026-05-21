int main(void) {
  unsigned long a = 100000UL;
  unsigned long b = 200UL;
  unsigned long r = a * b;
  return (int)(r >> 16);
}
