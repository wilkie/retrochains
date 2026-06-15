int g;
int next(void);

int try_read(void) {
  int x;
  if ((x = next()) != 0) {
    return x;
  }
  return -1;
}
