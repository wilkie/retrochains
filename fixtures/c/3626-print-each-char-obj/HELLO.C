int putchar(int c);

void print(char *s) {
  while (*s) putchar(*s++);
}
