int yes(void);
int no(void);
int pick(int flag) {
  if (flag) return yes();
  else return no();
}
