char buf[3];
void setAt(char *p, int i, char v) {
  p[i] = v;
}
int main(void) {
  setAt(buf, 1, 'X');
  return buf[1];
}
