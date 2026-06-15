struct Mixed {
  char tag;
  int count;
  long total;
  char *name;
};
int main(void) {
  struct Mixed m;
  m.tag = 'X';
  m.count = 42;
  m.total = 100000L;
  m.name = "hello";
  return m.count + (int)m.total + m.tag;
}
