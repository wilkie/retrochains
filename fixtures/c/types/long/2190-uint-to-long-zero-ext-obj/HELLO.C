int main(void) {
  unsigned int u = 0xFFFF;
  long l = (long)u;
  return (int)(l >> 16);
}
